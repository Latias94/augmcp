# augmcp（中文）

一个使用 Rust 与 rmcp SDK 实现的 MCP 服务器，提供“代码库增量索引 + 语义检索”能力。整体设计参考 acemcp（Python 版本），但运行在 Rust 生态（rmcp + axum）。

- 英文文档：README.md
- 参考实现（理念与接口）：https://github.com/qy527145/acemcp

## 特性

- 自动增量索引：搜索前自动进行增量索引（首轮全量，之后只上传变化）
- 尊重 .gitignore 与自定义排除规则
- 多编码读取：UTF‑8 → GBK → GB2312 → ISO‑8859‑1，必要时降级为 UTF‑8 lossy
- 大文件按行数切片（默认 800 行/块）
- 批量上传 + 指数退避重试
- 非流式检索：一次性返回格式化的文本结果
- 同时支持两种 MCP 传输：stdio 与基于 axum 的 streamable HTTP
- 可选 REST 接口，便于“索引+检索”一体化调用
- 数据、配置与日志集中存放在 `~/.augmcp`

## 安装

从源码构建：

```
# 克隆源码
git clone <your repo url>
cd augmcp

# 发布构建
cargo build --release
# 可选：安装到 cargo bin
cargo install --path .
```

## 配置

首次运行会自动创建 `~/.augmcp/settings.toml`，示例：

```
batch_size = 10
max_lines_per_blob = 800
# 生产环境请使用官方 Augment Code API
base_url = "https://d5.api.augmentcode.com/"
# 请勿提交个人密钥到仓库
token = "your-bearer-token-here"
text_extensions = [".py", ".js", ".ts", ...]
exclude_patterns = [
  ".venv", "venv", ".env", "env", "node_modules", ".git", "__pycache__",
  "*.pyc", "dist", "build", ".vscode", ".idea", "target", "bin", "obj",
]
```

命令行覆盖（优先级最高）：

```
# 本次运行覆盖
augmcp --base-url "https://d5.api.augmentcode.com/" --token "<TOKEN>" --transport stdio

# 覆盖并落盘
augmcp --persist-config --base-url "https://d5.api.augmentcode.com/" --token "<TOKEN>"
```

注意：
- Windows 请使用正斜杠路径：`C:/Users/name/project`
- 请勿提交个人 TOKEN 到代码仓库；密钥保存在 `~/.augmcp`

## 快速开始

1）一次性写入后端配置（写入 `~/.augmcp/settings.toml`）：

```
augmcp --persist-config \
  --base-url "https://d5.api.augmentcode.com/" \
  --token "<TOKEN>"
```

2）以 HTTP 模式运行（对外暴露 `/mcp` 与 REST 接口）：

```
augmcp --bind 127.0.0.1:8888
```

3）索引并绑定别名（可选，但推荐）：

```
curl -X POST http://127.0.0.1:8888/api/index \
  -H "Content-Type: application/json" \
  -d '{
    "project_root_path": "C:/Users/name/projects/myproj",
    "alias": "myproj"
  }'
```

4）按别名检索（若无索引会自动索引，有则直接检索）：

```
curl -X POST http://127.0.0.1:8888/api/search \
  -H "Content-Type: application/json" \
  -d '{
    "alias": "myproj",
    "query": "axum 路由定义和 HTTP 处理"
  }'
```

提示：无需 MCP/HTTP 的一次性本地验证：

```
augmcp --oneshot-path "C:/Users/name/projects/myproj" \
       --oneshot-query "查找日志配置"
```

## MCP 配置

Stdio（推荐）：

```
{
  "mcpServers": {
    "augmcp": {
      "command": "C:/path/to/augmcp.exe",
      "args": ["--transport", "stdio"]
    }
  }
}
```

HTTP（streamable，axum）：

```
{
  "mcpServers": {
    "augmcp": {
      "command": "C:/path/to/augmcp.exe",
      "args": ["--transport", "http", "--bind", "127.0.0.1:8888"]
    }
  }
}
```

### Claude Desktop 配置说明

- 优先使用 `stdio` 传输与 Claude Desktop 对接（兼容性最好）。
- 确保 `~/.augmcp/settings.toml` 中已配置 `base_url` 与 `token`（也可在 `args` 里透传）。
- Windows 示例：

```
{
  "mcpServers": {
    "augmcp": {
      "command": "C:/Users/name/.cargo/bin/augmcp.exe",
      "args": ["--transport", "stdio"]
    }
  }
}
```

## 工具（Tools）

### search_context
- 参数：
  - `project_root_path?`（string）：项目根目录（Windows 也用 `/`）
  - `alias?`（string）：已注册/绑定的别名（可选）
  - `skip_index_if_indexed?`（bool，默认 true）：若已有索引则直接检索
  - `query`（string）：检索问题
- 行为：若已有索引且允许跳过索引，直接检索；否则先增量索引再检索。

### index_project
- 参数：
  - `project_root_path?`（string）
  - `alias?`（string）：与路径一起提供时会绑定；仅提供 alias 可解析到已绑定路径
  - `force_full?`（bool，默认 false）：忽略缓存做全量
- 返回：统计文本（total/new/existing）。

## REST 接口（可选）

HTTP（默认）端点：

- `POST /api/search`
  - 请求：`{ "project_root_path"?: "...", "alias"?: "...", "query": "...", "skip_index_if_indexed"?: true }`
  - 行为：与 MCP 工具一致，若未索引会自动增量后检索

- `POST /api/index`
  - 支持 `{"async": true}` 后台索引，立即返回 `accepted`
  - 停止任务：`POST /api/index/stop`（按路径或别名）
  - 任务查询：`GET /api/tasks?project_root_path=...` 或 `?alias=...`（返回 running、progress、eta_secs）

- `GET /healthz`
  - 健康检查（标准 200 返回，JSON：`{ status: "ok", version: "..." }`）
  - 请求：`{ "project_root_path"?: "...", "alias"?: "...", "force_full"?: false }`
  - 返回：索引统计字符串

## 数据与日志

- 配置：`~/.augmcp/settings.toml`
- 项目索引：`~/.augmcp/data/projects.json`
- 别名表：`~/.augmcp/aliases.json`
- 日志：`~/.augmcp/log/augmcp.log`（按日滚动）

日志会记录：入口、文件收集与切片、增量统计、上传、索引落盘、检索开始/结束，便于追踪“先索引后搜索”的完整链路。

## 工作原理（简述）

1. 收集文本文件（尊重 `.gitignore` 与排除规则）。
2. 多编码读取并按行切片；计算 `sha256(path+content)` 作为 blob 标识。
3. 与本地 `projects.json` 比较，仅上传新增 blob 到 `{base_url}/batch-upload`。
4. 调用 `{base_url}/agents/codebase-retrieval`，使用全量 blob 列表检索，返回 `formatted_retrieval`。

## 参考链接

- acemcp（Python 版本理念与接口）：https://github.com/qy527145/acemcp
