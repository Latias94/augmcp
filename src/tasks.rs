use parking_lot::Mutex;
use serde::Serialize;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Default)]
pub struct TaskProgress {
    pub phase: String,
    pub total: usize,
    pub new_total: usize,
    pub uploaded: usize,
    pub chunks_total: usize,
    pub chunk_index: usize,
    pub chunk_bytes: usize,
    pub percent: f32,
    pub started_at: u64,
    pub updated_at: u64,
    pub message: Option<String>,
}

impl TaskProgress {
    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
    pub fn new_start() -> Self {
        Self {
            phase: "starting".into(),
            started_at: Self::now(),
            updated_at: Self::now(),
            ..Default::default()
        }
    }
}

#[derive(Clone, Default)]
pub struct TaskManager {
    statuses: Arc<Mutex<HashMap<String, TaskProgress>>>,
    handles: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin(&self, key: &str) -> bool {
        let mut map = self.statuses.lock();
        if map.contains_key(key) {
            return false;
        }
        map.insert(key.to_string(), TaskProgress::new_start());
        true
    }

    pub fn set_handle(&self, key: &str, h: tokio::task::JoinHandle<()>) {
        self.handles.lock().insert(key.to_string(), h);
    }

    pub fn set_phase(&self, key: &str, phase: &str) {
        if let Some(st) = self.statuses.lock().get_mut(key) {
            st.phase = phase.to_string();
            st.updated_at = TaskProgress::now();
        }
    }

    pub fn set_upload_totals(
        &self,
        key: &str,
        new_total: usize,
        chunks_total: usize,
        total: usize,
    ) {
        if let Some(st) = self.statuses.lock().get_mut(key) {
            st.new_total = new_total;
            st.chunks_total = chunks_total;
            st.total = total;
            st.updated_at = TaskProgress::now();
        }
    }

    pub fn on_chunk(&self, key: &str, uploaded: usize, chunk_index: usize, chunk_bytes: usize) {
        if let Some(st) = self.statuses.lock().get_mut(key) {
            st.phase = "uploading".into();
            st.uploaded = uploaded;
            st.chunk_index = chunk_index;
            st.chunk_bytes = chunk_bytes;
            st.percent = if st.new_total == 0 {
                100.0
            } else {
                (uploaded as f32) * 100.0 / (st.new_total as f32)
            };
            st.updated_at = TaskProgress::now();
        }
    }

    pub fn finish(&self, key: &str) {
        if let Some(st) = self.statuses.lock().get_mut(key) {
            st.phase = "done".into();
            st.percent = 100.0;
            st.updated_at = TaskProgress::now();
        }
        self.handles.lock().remove(key);
    }

    pub fn fail(&self, key: &str, msg: String) {
        if let Some(st) = self.statuses.lock().get_mut(key) {
            st.phase = "failed".into();
            st.message = Some(msg);
            st.updated_at = TaskProgress::now();
        }
        self.handles.lock().remove(key);
    }

    pub fn abort(&self, key: &str) -> bool {
        if let Some(h) = self.handles.lock().remove(key) {
            h.abort();
            if let Some(st) = self.statuses.lock().get_mut(key) {
                st.phase = "aborted".into();
                st.updated_at = TaskProgress::now();
            }
            return true;
        }
        false
    }

    pub fn is_running(&self, key: &str) -> bool {
        self.handles.lock().contains_key(key)
    }

    pub fn get(&self, key: &str) -> Option<TaskProgress> {
        self.statuses.lock().get(key).cloned()
    }
}
