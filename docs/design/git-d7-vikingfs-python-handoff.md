# D7 移交文档:VikingFS Python 层 + URI ↔ tree path

**给新智能体的任务说明 —— 假设你对本仓库与 git 版本控制项目都没有上下文。**

读完这篇文档,你应该能够独立完成 D7 阶段的所有工作。

---

## 0. 一句话目标

把已经写好的 Rust PyO3 binding(`git_commit / git_restore / git_show`)封装成 Python 用户能直接调用的 `VikingFS.commit / restore / show / log` 方法,完成 OpenViking 多版本管理特性从 Rust 引擎到 Python 业务层的最后一公里。

---

## 1. 项目背景(必读)

### 1.1 OpenViking 是什么
- `viking://` URI 起头的双层存储抽象
- **上层**:Python `VikingFS`(`openviking/storage/viking_fs.py`)——做 URI 规范化、加密、L0/L1 摘要、向量同步、租户隔离
- **下层**:Rust `RAGFS`(`crates/ragfs/`)+ PyO3 binding(`crates/ragfs-python/`)——`FileSystem` trait + `MountableFS` radix-trie 路由,实际数据落到 `localfs`/`s3fs`/`memfs` 等插件

### 1.2 Git 版本控制项目目标
在 OpenViking 上嵌入 in-process Git 服务,给每个 `account_id` 一个逻辑 Git 仓库,跨 scope 共享同一棵 root tree,提供 `commit / restore / show` 三个原语。

### 1.3 截至 D7 之前已交付的内容

| 阶段 | 模块 | 状态 |
|---|---|---|
| D1-D2 | `ObjectStore` / `RefStore` trait + Local + S3 后端 | ✅ |
| D3 | `GitService::commit` | ✅ |
| D4 | `GitService::show` | ✅ |
| D5 | `GitService::restore`(以 HEAD 为 parent 正向生成新 commit)| ✅ |
| D6 | PyO3 binding(`git_commit / git_restore / git_show` 方法挂在 `RAGFSBindingClient` 上)| ✅ |
| **D7** | **VikingFS Python 层 + URI ↔ tree path 转换** | **你现在做** |

### 1.4 完整设计文档
**强烈推荐先通读** `docs/design/git-version-control-design.md`。重点章节:
- §1-3:背景 / 决策 / 架构
- §4:Tree 布局与路径剪枝(决定哪些路径会被 git 追踪)
- §8.2:restore 算法(理解 binding 的语义)
- **§9.1**:PyO3 binding 方法签名(D6 已完成)
- **§9.2**:VikingFS 4 个 Python 方法签名(**你要实现**)
- **§12.2**:加密 ⚠️ **D7 最大坑**
- §15.2:测试用例清单

---

## 2. 你要做什么(交付清单)

### 2.1 Python 代码

在 `openviking/storage/viking_fs.py` 的 `VikingFS` 类上**追加** 4 个公开 async 方法:

```python
async def commit(self, *, message, paths=None, branch="main",
                 author_name=None, author_email=None, ctx=None) -> dict
async def restore(self, *, project_dir, source_commit, branch="main",
                  dry_run=False, message=None,
                  author_name=None, author_email=None, ctx=None) -> dict
async def show(self, target_ref, *, path=None, ctx=None) -> dict | bytes
async def log(self, *, branch="main", limit=20, ctx=None) -> list[dict]
```

**实现规则**(参考文件中已有 `read/write/rm/ls` 方法的模式):

1. **拿 `ctx`**:`real_ctx = self._ctx_or_default(ctx)`,从 `real_ctx.account_id` 取 account
2. **URI → tree path**:通过新增的 `_uri_to_tree_path` 工具方法转换
3. **调 binding**:通过 `self._async_agfs.run("git_commit", account=..., ...)`(`async_client.py:30` 已暴露 `run` 方法,支持 `**kwargs`)
4. **错误**:让 binding 抛出的 `AGFSClientError` / `AGFSNotFoundError` / `AGFSNotSupportedError` / `GitConcurrentCommitError` 自然传播到调用方;**不要**包成新异常类

### 2.2 工具方法:URI ↔ tree path 转换

`viking://` URI 的格式是 `viking://{scope}/{rest}`(`scope ∈ {resources, user, session, queue, temp, upload}`),tree path 是 **账号内相对路径** —— 形如 `"resources/proj_a/docs/a.md"`(没有 `viking://` 前缀,也没有 `/local/{account}/` 前缀)。

```python
def _uri_to_tree_path(self, uri: str) -> str:
    """viking://resources/a.md → 'resources/a.md'。

    内部 scope(_system/tasks/temp/queue/upload)应拒绝 ——
    它们在 Rust 端会被 enumerate.rs::prune_path 剪掉,
    传进去也是无效操作。
    """

def _tree_path_to_uri(self, tree_path: str) -> str:
    """'resources/a.md' → 'viking://resources/a.md'。"""
```

⚠️ **scope 校验**:
- 已有 `VikingURI.INTERNAL_SCOPES = frozenset({"temp", "queue", "upload"})`(`openviking_cli/utils/uri.py:41`)
- Rust 端 `prune_path`(`crates/ragfs/src/git/enumerate.rs:26`)剪除的 first segment 是 `{"_system", "tasks", "temp", "queue", "upload"}`
- **请以 Rust 端的清单为准**,加上 `.path.ovlock`,在 `_uri_to_tree_path` 里遇到这些 scope 抛 `ValueError`

### 2.3 向量索引重建 hook

restore 完成后,Rust 端会把受影响路径写回 VFS,但**向量索引不在版本管理范围内**(`.faiss`/`.index`/`embedding_cache/` 已被 prune)。需要异步重建。

参考 `openviking/service/reindex_executor.py` 的 `ReindexExecutor.execute(uri, mode="vectors_only", wait=False, ctx)` API。重建失败不应阻塞 restore 返回 —— 用 `asyncio.create_task` 起后台任务,在外层加 `try/except` 记 `logger.exception`。

派生文件(`.abstract.md` / `.overview.md` / `.relations.json`)**跳过**重建 —— 它们已经被 git 一起回滚了。

### 2.4 测试

在 `tests/agfs/` 下新建 `test_viking_fs_git.py`,覆盖以下用例(对应设计文档 §15.2):

| # | 用例 | 必须 |
|---|---|---|
| 1 | commit → show 闭环 | ✅ |
| 2 | restore 闭环(VFS 内容回滚 + HEAD 前进 + 新 commit parent=旧 HEAD)| ✅ |
| 3 | dry_run 不改 VFS、不动 ref | ✅ |
| 4 | **跨 scope 原子性**:一次 commit 改 `viking://resources/a.md` + `viking://user/skills/b.py`,restore 父 commit 后两者都回滚 | ✅ |
| 5 | **派生文件纳入**:`.abstract.md` 跟着源文件一起被 commit / 一起 restore | ✅ |
| 6 | **账号隔离**:account A 的 commit_oid 在 account B 下 show 抛 `AGFSNotFoundError` | ✅ |
| 7 | **双重加密**:启用 `_encryptor` 的情况下,restore 后 `viking_fs.read` 拿到的明文与 commit 前一致(见 §3.1)| ✅ **必跑** |
| 8 | URI 转换边界:internal scope 抛 `ValueError` | ✅ |
| 9 | `log` 沿 parent 链反向遍历正确 | ✅ |
| 10 | feature disabled 时方法抛 `AGFSNotSupportedError` | ✅ |
| 11 | 大文件(80MB)commit / restore / show 字节一致 | 推荐 |

参考已有的 Rust binding 集成测 `tests/agfs/test_git_binding.py` 的 fixture 写法 —— 它构造了临时 `ragfs.toml` + localfs mount + `RAGFSBindingClient`。

---

## 3. 关键陷阱(必读)

### 3.1 ⚠️ 双重加密(最大坑)

**问题**:`VikingFS.write`(`viking_fs.py:417`)在写入前调 `self._encrypt_content(data)`。

- **commit 时**:Rust binding 从 VFS 读出来的是**密文**(因为之前 `viking_fs.write` 加密过)。Git 在版本管理密文,这是正确的、设计文档接受的行为。
- **restore 时**:Rust 端 `GitService::restore` 把 blob 写回 VFS —— 它注入的是 `Arc<dyn FileSystem>` = `MountableFS`,**不经过 `VikingFS.write` 的加密层**。所以密文原样写回 → 用 `viking_fs.read` 读出时会自动解密 → **得到正确的明文**。

**你的责任**:
- **不要**在 D7 Python 层做任何加密绕过(Rust 端已经走对路径了)
- **必须**写一个集成测验证整条链路:enable encryptor → write 明文 → commit → 改文件 → restore → read → 拿到原始明文。这就是设计文档 §15.2 #9 用例。

### 3.2 path 列表的 URI 形式

`VikingFS.commit` 的 `paths` 参数是 `viking://` URI 列表,但 binding 端 `git_commit` 的 `paths` 参数是账号内 tree 路径列表。**必须做 URI → tree path 转换**。空列表与 `None` 不等价 —— `None` 表示"全量遍历 VFS",`[]` 在 binding 里实际等价于"显式空列表 → 什么都不 commit"。建议:`tree_paths = [self._uri_to_tree_path(p) for p in paths] if paths else None`。

### 3.3 `project_dir` 的传值

`VikingFS.restore` 的 `project_dir` 输入可能是:
- `"viking://resources/proj_a"` (带 scheme + 尾斜杠或无)
- `"viking://resources/proj_a/"`
- `"resources/proj_a"`(已经是 tree path)

binding 端要求是 tree path 且**不带前后斜杠**。`_uri_to_tree_path(p).rstrip("/")` 处理即可。空 / 根 / internal scope 应抛 `ValueError`(对应 Rust 端 `InvalidProjectDir` 错误)。

### 3.4 `show` 的返回类型

binding 的 `git_show` 在 `path=None` 时返回 commit 元数据 dict;在 `path=str` 时返回 `{"oid","size","bytes": PyBytes}` 三键字典。

为了让 Python 调用方更顺手,`VikingFS.show` 在 `path` 非 None 时**直接返回 bytes**(剥掉 oid/size 包装),`path=None` 时返回 dict。签名:`async def show(target_ref, *, path=None) -> dict | bytes`。

### 3.5 `RAGFSBindingClient` 必须以 `config_path` 构造

Rust 端 `RAGFSBindingClient.__init__(config_path=None)` 是 D6 修改的签名。当 `config_path` 为 `None` 或文件里没有 `[git]` 段时,`git_service` 为 `None`,调用 `git_commit/git_restore/git_show` 会抛 `AGFSNotSupportedError`。

**对 `VikingFS` 的影响**:`VikingFS` 持有的 `agfs` 是外部传入的,**D7 不要修改 `VikingFS.__init__`**。git feature 是否可用由部署侧的 `ragfs.toml` 是否配 `[git]` 段决定。`VikingFS.commit/restore/show` 调用如果遇到 `AGFSNotSupportedError`,自然往上抛即可。

---

## 4. 当前代码现状(具体路径)

### 4.1 Rust 侧(已完成,只读参考)

| 文件 | 角色 |
|---|---|
| `crates/ragfs/src/git/service.rs` | `GitService::{commit, restore, show}` 主流程 |
| `crates/ragfs/src/git/types.rs` | `CommitRequest/Response`、`RestoreRequest/Response`、`ShowRequest/Response` DTO |
| `crates/ragfs/src/git/error.rs` | `GitError` 枚举 |
| `crates/ragfs/src/git/enumerate.rs:26` | `INTERNAL_FIRST_SEGMENTS`、`prune_path` 路径剪枝规则 |
| `crates/ragfs/src/git/config.rs` | `GitConfig` serde 结构 |
| `crates/ragfs-python/src/lib.rs` | `RAGFSBindingClient` 类,新增了 `git_service`/`git_backend` 字段、`load_git_from_config`、`git_commit/git_restore/git_show` 三个 pymethod、health() 字段 |
| `crates/ragfs-python/src/git.rs` | 后端构造、错误映射、kwargs 解析、响应转 dict |

**对你的关注点**:
- Rust binding 调用约定(`tests/agfs/test_git_binding.py` 是最直接的参考)
- `parse_commit_request` 等的必填字段(`account, branch, message, author_name, author_email`),`paths` 可选
- `restore` 必填:`account, branch, project_dir, source_commit, author_name, author_email`,可选 `dry_run, message`
- `show` 必填:`account, target_ref`,可选 `path`

### 4.2 Python 侧(你要改的)

| 文件 | 角色 |
|---|---|
| `openviking/storage/viking_fs.py`(2666 行) | `VikingFS` 类,行 230 起。要在末尾追加 4 方法 + 2 工具方法 |
| `openviking_cli/utils/uri.py` | `VikingURI` 类,`LISTABLE_SCOPES` (行 35) / `INTERNAL_SCOPES` (行 41) 在此 |
| `openviking/pyagfs/async_client.py:30` | `AsyncAGFSClient.run(method_name, **kwargs)` —— **直接调** binding 的入口 |
| `openviking/pyagfs/exceptions.py:138` | `GitConcurrentCommitError` 已新增 |
| `openviking/service/reindex_executor.py:89` | `ReindexExecutor.execute(uri, mode, wait, ctx)` —— restore 后调它 |
| `openviking/server/identity.py` | `RequestContext`、`account_id` 在 `ctx.account_id`(行 1481 起的 `_ctx_or_default` 模式)|
| `tests/agfs/test_git_binding.py` | binding 端测试参考(fixture 怎么搭、断言怎么写)|

### 4.3 现有 `VikingFS.write` 关键片段(对照你新方法的写法)

```python
# viking_fs.py 行 415-428
async def write(self, uri, data, ctx=None):
    self._ensure_mutable_access(uri, ctx)
    path = self._uri_to_path(uri, ctx=ctx)            # ← URI → /local/{account}/X
    if isinstance(data, str):
        data = data.encode("utf-8")
    data = await self._encrypt_content(data, ctx=ctx)
    return await self._async_agfs.write(path, data)   # ← 调 binding
```

你的新方法要照搬 `ctx + _uri_to_xxx + _async_agfs.run` 这个套路,但**不要**对 `data` 调加密(commit/restore 不接 raw data)。

---

## 5. 推荐实现顺序

1. **第一步:URI 转换 + 单测**
   - 在 `VikingFS` 上加 `_uri_to_tree_path` / `_tree_path_to_uri`
   - 单测覆盖:所有合法 scope、internal scope 抛错、根 URI 抛错、相对路径直通、尾斜杠处理
   - 这一步纯函数,不依赖 binding,可以独立写完测完

2. **第二步:`show` + `log`(纯读路径)**
   - 最容易:无写入、无加密、无副作用
   - 先做 `show`,再用 `show` 拼出 `log`
   - 在 `tests/agfs/test_viking_fs_git.py` 验证 commit → show 闭环(commit 通过 binding 直接调,不必等 `VikingFS.commit`)

3. **第三步:`commit`**
   - paths URI → tree path 转换
   - author 默认值的处理(可以从配置读,首版直接用占位符 `"viking-bot"` / `"bot@viking.local"`,与 §10.1 的默认对齐)

4. **第四步:`restore`**
   - 同上 URI 转换
   - 加 vector 重建 hook
   - **务必**写双重加密用例(§3.1)—— 这是 D7 必须实证的核心安全属性

5. **第五步:跨 scope / 隔离 / disabled 用例**

每写完一步就跑测试,逐步推进。

---

## 6. 验证方式

### 6.1 构建 binding(每次改完 Rust 都要)

不需要 —— D6 已经写完,D7 不应改 Rust。如果发现确实需要(参考 §7),按以下命令构建:

```bash
source /home/byteide/envs/openviking/bin/activate
cd crates/ragfs-python && maturin develop --release
```

### 6.2 跑 Python 测试

```bash
source /home/byteide/envs/openviking/bin/activate
cd /cloudide/workspace/OpenViking
python3 -m pytest tests/agfs/test_viking_fs_git.py -v --no-header -o "addopts="
```

注意 `addopts=""` —— 项目 `pyproject.toml` 的 `addopts` 带 `--cov`,本地没装 coverage 插件,会失败。

### 6.3 跑 Rust binding 测(回归保险)

```bash
source /home/byteide/envs/openviking/bin/activate
LD_LIBRARY_PATH=/opt/tiger/pyenv/versions/3.12.4/lib:$LD_LIBRARY_PATH \
PYO3_PYTHON=/home/byteide/envs/openviking/bin/python3 \
cargo test -p ragfs-python --no-default-features

python3 -m pytest tests/agfs/test_git_binding.py -v --no-header -o "addopts="
```

期望:**19 Rust 单测 + 10 Python 集成测全过**(D6 完成时的基线)。

---

## 7. 你可能需要的小幅 Rust 改动(可选)

设计文档 §9.2 提到 restore 完成后用 `result.get("affected_files", [])` 触发向量重建。但**当前** `RestoreResponse::Applied` 输出的是 `written`/`deleted` **整数计数**,不带具体路径:

```python
# 当前返回:
{
  "result": "applied",
  "new_commit_oid": "...",
  "source_commit": "...",
  "parent_commit": "...",
  "written": 3,        # ← 只是数字
  "deleted": 1,
  "unchanged": 5,
}
```

**你有两个选择**:

**A. (推荐) 改 Rust 端**:在 `crates/ragfs/src/git/types.rs::RestoreResponse::Applied` 加 `written_paths: Vec<String>` / `deleted_paths: Vec<String>` 字段(`GitService::restore` 内部已经有这些路径,只是 `len()` 丢了)。修改约 30 行,加 2-3 个单测,然后改 `restore_response_to_pydict` 输出对应键。Python 侧直接遍历该列表触发重建。

**B. Python 自己枚举**:restore 返回后,Python 调一次 `ls(project_dir)` 拿当前路径列表,逐个触发重建。**缺点**:会把"被 restore 删掉"的路径漏掉(因为 ls 看不到了),且做了无谓的 IO。

**推荐 A**,改动小、语义对、binding 接口更对称。如果选 A,改完记得重跑 D6 的所有测试。

---

## 8. 验收清单

- [ ] 4 个 `VikingFS` 方法 + 2 个 URI 转换工具方法实现
- [ ] `tests/agfs/test_viking_fs_git.py` 覆盖 §2.4 表里全部"必须"用例
- [ ] **双重加密用例真实跑通(开启 encryptor)**
- [ ] 跨 scope 原子性用例真实跑通(一个 commit 改两个 scope 各一个文件)
- [ ] D6 的 Rust 单测(19)+ binding Python 集成测(10)全部回归通过
- [ ] `VikingFS` 类原有方法签名 / 行为没有任何变化
- [ ] 没有引入新的 Python 异常类(复用 `pyagfs.exceptions` 已有的)
- [ ] 代码风格与 `viking_fs.py` 中现有方法一致(类型提示、`ctx=None` 参数位置、log 用法)

---

## 9. 不要做什么(超出 D7 范围)

- ❌ 不要改 `VikingFS.write` / `read` / `rm` 等已有方法
- ❌ 不要给 `viking_fs.write` 加 `raw=True` 参数(设计文档提过这个备选,但 Rust 端的 restore 已经走 `MountableFS` 绕过加密了,Python 层不需要再做)
- ❌ 不要实现 D9 的 tracing/metrics(那是下一阶段)
- ❌ 不要做 branch 管理 / merge / GC / pack file 等长尾功能
- ❌ 不要修改 PyO3 binding 的方法签名(除 §7 提到的 `RestoreResponse` 字段扩展外)
- ❌ 不要把 git 数据暴露到 `viking://` 命名空间下

---

## 10. 卡住时去哪里找答案

- 设计语义:`docs/design/git-version-control-design.md`
- Rust 端实现细节:`crates/ragfs/src/git/service.rs`(`restore` 在 L286 起)
- Binding 用法示例:`tests/agfs/test_git_binding.py`
- Python 现有方法对照:`openviking/storage/viking_fs.py` 行 415 (`write`)、行 471 (`rm`)、行 1340 (`stat`)
- URI 工具:`openviking_cli/utils/uri.py::VikingURI`
- 异步 binding 桥:`openviking/pyagfs/async_client.py::AsyncAGFSClient.run`

---

## 11. 提交规范

参考项目 `git log` 风格,小步提交,每个 commit 单一职责。建议拆分:

1. `feat(viking_fs): add _uri_to_tree_path and _tree_path_to_uri`
2. `feat(viking_fs): add git show + log methods`
3. `feat(viking_fs): add git commit method`
4. `feat(viking_fs): add git restore method with vector reindex hook`
5. `test(viking_fs/git): commit + show + log roundtrip`
6. `test(viking_fs/git): restore + dry_run + cross-scope atomicity`
7. `test(viking_fs/git): double-encryption end-to-end`
8. `(可选) feat(git/restore): expose written_paths/deleted_paths in Applied response`

每个 commit 之前确保对应测试通过。最后一次提交完毕后,跑完整测试套件回归。
