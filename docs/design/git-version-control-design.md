# OpenViking Git 版本控制设计文档

**一句话摘要**：在现有 OpenViking 的 RAGFS Rust 实现中嵌入一套基于 `gitoxide` 的 in-process Git 服务，以 **账号(account_id)粒度** 提供 `commit / restore / show` 三个版本管理原语;通过 PyO3 binding 直接被 `VikingFS` Python 层调用,全程零 HTTP、零额外进程,Git 对象/Ref 后端复用现有 `localfs`/`s3fs` 客户端,实现"本地或远程"对称配置。

# 1. 背景与目标
## 1.1 业务背景
OpenViking 现有存储架构是一套以 `viking://` URI 为入口的双层抽象:上层 `VikingFS`(Python)负责 URI 规范化、L0/L1 摘要、向量同步、租户隔离;下层 RAGFS(Rust + PyO3 binding)提供 `FileSystem` trait 与 `MountableFS` radix-trie 路由,实际数据落到 `localfs`、`s3fs`、`memfs` 等插件后端。
在持续运行过程中,用户/Agent 对 `viking://resources/`、`viking://agent/skills/` 等命名空间的写入是连续且不可逆的——出错后无法回滚,跨多个文件的"逻辑事务"难以原子化捕获,实验性改动需要手动备份。这些场景的本质需求都是一套**面向账号的多版本快照机制**,语义与 Git 的 commit/restore/show 高度同构。
## 1.2 设计目标
- **显式版本化**:用户/Agent 通过 API 显式触发 commit/restore/show,不引入隐式 hook,避免影响现有写链路的延迟与一致性语义
- **账号粒度仓库**:每个 `account_id` 一个逻辑 Git 仓库,跨 scope (resources/agent/user/session) 共享同一棵 root tree,支持跨 scope 的原子快照
- **多后端对称**:Git objects / refs 的实际存储类型与 resources 目录一致,可在配置中切换本地(local)或远程(s3),运维心智零增量
- **零进程膨胀**:Git 服务以 in-process binding 形式嵌入现有 RAGFS,共享 Tokio runtime 与配置加载链路,不引入新 HTTP server
- **对现有代码侵入最小**:不修改 `content_write.py`、`viking_fs.write/rm/mv` 等核心写链路,仅在 `VikingFS` 上增加 3 个新方法
- ***定向恢复 (restore)****: 支持以 *`*(project_dir, commit_id)*`* 为输入，将指定 project 目录恢复到目标 commit 的快照状态，并以 HEAD 为父节点****正向生成一个新 commit****。非目标 project 目录保持当前最新状态不动。*
## 1.3 非目标 (Out of Scope)
- 不实现自动 commit hook (首版纯主动 API 触发)
- 不实现分支 merge / rebase / cherry-pick / push/pull (首版只覆盖快照 + 回滚 + 查看)
- 不暴露 Git 数据到 `viking://` 用户命名空间 (避免被用户误删/误改)
- 不支持向量索引数据的版本化 (向量索引由 watcher 异步重建, restore 后需触发重建;L0/L1 派生文件已纳入版本管理)
- 不支持 ref 回退式 checkout*: 本方案不提供 "把 main / HEAD 指针直接移动到旧 commit" 的能力。所有恢复操作都通过正向新增 commit 实现，保证 HEAD 单调前进、commit 链完整可审计。如需查看旧版本，使用 *`*show*`* 接口的只读路径。*
---

# 2. 核心设计决策
以下三条决策是本方案的**不可变约束**,所有实现细节都从这三条推导而来。

      决策

      设计含义

      替代方案被淘汰的原因

      **单 Repo per account_id**

      同一账号下的 `resources/`、`agent/`、`user/`、`session/` 全部在一棵 root tree 之下;一次 commit 可覆盖任意 scope 的子集

      per-resource repo 会产生 N×账号数量的索引数据,跨 resource 的"事务性快照"需要协调多 repo,复杂度高

      **纯 API 触发,不接 hook**

      `content_write.py` / `viking_fs.write/rm/mv` 完全不动;Git 仅通过 `VikingFS.commit/restore/show` 三个新方法被显式调用

      hook 模式会让每次小写入都触发 Git 写入,放大延迟、放大冲突窗口、放大 ref CAS 失败率;首版优先简单

      **Git 存储后端与 resources 同构**

      定义 `ObjectStore` / `RefStore` trait,提供 local 与 s3 两种实现,直接复用 `plugins::localfs::LocalFileSystem` 和 `plugins::s3fs::S3Client`

      独立实现 Git 存储后端会重复造轮子;走 `MountableFS` 又会让 Git 数据进入用户命名空间

      **嵌入为 **`**crates/ragfs**`** 子模块**

      新增 `crates/ragfs/src/git/` 模块,与 `core/`、`plugins/`、`server/` 平级;PyO3 binding 在 `RAGFSBindingClient` 上加 3 个方法

      独立 crate 会引入额外配置、额外 runtime、额外鉴权;`ServicePlugin` 又无法表达 commit 这种非文件操作的语义

      **暴露方式 = PyO3 binding,非 HTTP**

      三个新方法挂在现有 `RAGFSBindingClient` 上,通过 `AsyncAGFSClient.run` 由 `VikingFS` 调用,与 `ls/read/write` 一致

      HTTP server 路径在 OpenViking 当前架构中已是 legacy,生产路径是 in-process binding

---

# 3. 整体架构
## 3.1 分层与依赖关系
```
┌─────────────────────────────────────────────────────────┐
│              VikingFS (Python)                          │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌──────────┐  │
│  │  read   │  │  write  │  │  commit │  │ checkout │  │
│  └─────────┘  └─────────┘  └─────────┘  └──────────┘  │
└─────────────────────────────────────────────────────────┘
                           │
                           ▼
┌─────────────────────────────────────────────────────────┐
│        RAGFS (Rust) + PyO3 Binding                      │
│  ┌───────────────────────────────────────────────────┐ │
│  │  GitService                                       │ │
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐       │ │
│  │  │  commit  │  │ checkout  │  │   show   │       │ │
│  │  └──────────┘  └──────────┘  └──────────┘       │ │
│  └───────────────────────────────────────────────────┘ │
│  ┌───────────────────────────────────────────────────┐ │
│  │  ObjectStore / RefStore Traits                    │ │
│  │  ┌──────────────┐       ┌──────────────┐        │ │
│  │  │ LocalBackend │       │   S3Backend  │        │ │
│  │  └──────────────┘       └──────────────┘        │ │
│  └───────────────────────────────────────────────────┘ │
│  ┌───────────────────────────────────────────────────┐ │
│  │  MountableFS (现有，不动)                          │ │
│  └───────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

## 3.2 数据流(三个核心命令)

**commit**: `VikingFS.commit()` → `RAGFSBindingClient.git_commit()` → `GitService::commit()` → `ObjectStore::put()` (blob/tree/commit) → `RefStore::cas_update()`

**restore**: `VikingFS.restore()` → `RAGFSBindingClient.git_restore()` → `GitService::restore()` → `ObjectStore::get()` (tree/blob) → `MountableFS::write/rm`

**show**: `VikingFS.show()` → `RAGFSBindingClient.git_show()` → `GitService::show()` → `ObjectStore::get()` (commit/tree/blob)

## 3.3 关键设计原则

        **Git 数据不进 viking 命名空间**
    Git 模块直接持有 `LocalFileSystem`/`S3Client` 实例,**不**通过 `MountableFS` 路由。Git 数据存到 `git/{account}/objects/...`,用户在 `viking://` 下看不到、也改不到。

        **restore 写回走完整 VFS**
    restore 是显式触发的批量写,必须经过 `viking_fs.write/rm` 触发现有 lock、加密。L0/L1 派生文件随源文件一起回滚,无需重建。restore 完成后通过 `VikingFS._trigger_vector_rebuild(paths)` hook 触发向量索引异步重建。这是 Git 模块唯一主动调用 `MountableFS` 的地方,方向单一无循环。
    回写完成后，以**当前 HEAD 为 parent**、用回写后的工作区生成**新 commit**(message 默认 `"restore  from "`),CAS 更新 ref,HEAD 单调前进。HEAD 永远只前进、不回退，审计链完整。

---

# 4. Repo 边界与 Tree 布局
## 4.1 Tree 镜像 VikingFS 命名空间
由于 `viking_fs._uri_to_path` 已经定义了 `viking://X → /local/{account_id}/X` 的映射规则,我们让 Git 的 root tree 完全镜像 `/local/{account_id}/` 下的子目录结构。这样 tree path 与 viking URI 后缀一一对应,语义直观、无歧义。
## 4.2 路径剪枝(自动排除)

| 类别       | 条目                                                               | 理由                                                 |
| -------- | ---------------------------------------------------------------- | -------------------------------------------------- |
| 内部目录     | `_system/`, `tasks/`, `.path.ovlock`                             | 与 `VikingFS._INTERNAL_NAMES` 一致，均为运行时锁/系统状态，不应纳入版本 |
| 向量索引     | 向量索引文件 (`.faiss`, `.index`, embedding cache 等)                   | 向量索引为纯计算产物，体积大且可由源文件重新生成，纳入版本历史无意义且浪费存储            |
| 临时 scope | `viking://temp/...`, `viking://queue/...`, `viking://upload/...` | 属于 `INTERNAL_SCOPES`，本身不持久；commit 时跳过              |

L0/L1 派生文件(`.abstract.md`、`.overview.md`、`.relations.json`)已纳入主线 commit,restore 时随源文件一起回滚,无需重新生成。向量索引数据在 restore 完成后由 VikingFS hook 触发异步重建。
## 4.3 单库多命名空间的优势
1. **原子跨 scope 快照**:一次 commit 可同时覆盖 `resources/docs` 和 `agent/skills`,对应"Agent 一次任务的所有产出"这种逻辑事务
1. **定向回滚**:restore 时可指定 `paths=["resources/docs/auth.md"]`,只回滚单个文件
1. **索引数据线性**:objects/refs 数量随账号线性,不随 resource 数量指数膨胀
1. **权限边界清晰**:account_id 已经是天然的隔离单位,Git 仓库边界与现有权限模型完全对齐
---

# 5. 物理布局
## 5.1 Crate 目录结构
Git 模块作为 `crates/ragfs` 的子模块,与 `core/`、`plugins/`、`server/` 平级。新增文件全部位于 `crates/ragfs/src/git/` 下,Python binding 仅在 `crates/ragfs-python/src/lib.rs` 上追加方法,无新 crate。
```plaintext
crates/ragfs/src/
├── core/                       # 既有(不动)
├── plugins/                    # 既有(不动)
├── server/                     # 既有(不动)
└── git/                        # 新增
    ├── mod.rs                  # 模块入口 + 重导出
    ├── service.rs              # GitService(commit/restore/show 主流程)
    ├── object_store.rs         # ObjectStore trait + 错误类型
    ├── ref_store.rs            # RefStore trait + 错误类型
    ├── tree_builder.rs         # gix_object::tree::Editor 封装
    ├── commit.rs               # CommitBuilder / 签名 / 时间戳
    ├── restore.rs             # restore 差异计算与回写 VFS
    ├── show.rs                 # ref → commit → tree → blob 解析
    ├── enumerate.rs            # 从 MountableFS 枚举 + 剪枝
    ├── types.rs                # 请求/响应 DTO + Oid 封装
    ├── error.rs                # GitError(thiserror) + From
    ├── config.rs               # GitConfig(serde) + 加载
    └── backends/
        ├── mod.rs
        ├── local.rs            # LocalObjectStore / LocalRefStore(复用 LocalFileSystem)
        └── s3.rs               # S3ObjectStore / S3RefStore(复用 S3Client + If-Match)

crates/ragfs-python/src/
└── lib.rs                      # 追加 git_commit / git_restore / git_show 方法

openviking/openviking/storage/
└── viking_fs.py                # 追加 commit / restore / show / log + URI↔tree-path 工具
```

## 5.2 依赖增量
仅引入 gitoxide 中实现 commit/restore/show MVP 所需的最小子 crate 集合,通过 `crates/ragfs/Cargo.toml` 增量声明:

```toml
[dependencies]
# === Git (gitoxide) ===
gix-hash       = "0.14"   # ObjectId / Hash 抽象
gix-object     = "0.42"   # Blob/Tree/Commit 编解码 + tree::Editor
gix-features   = { version = "0.38", features = ["zlib"] }  # zlib loose-object 压缩
gix-actor      = "0.31"   # 作者/提交者签名(name  ts tz)
gix-date       = "0.8"    # 时间戳格式化
gix-validate   = "0.8"    # ref 名校验(避免注入)

# === S3 后端复用 ===
# 已存在: aws-sdk-s3 / aws-config(由 plugins/s3fs 引入,Cargo workspace 共享)

[dev-dependencies]
loom           = "0.7"    # RefStore CAS 并发模型测试
tempfile       = "3"
proptest       = "1"      # tree_builder 路径剪枝 fuzz
```

💡 **说明:** 不引入 `gitoxide` 顶层 crate,只挑选 commit/restoreshow MVP 必需的子 crate;不引入 `gix-pack`(MVP 只用 loose object 格式)、不引入 `gix-protocol`(无 push/pull 需求)、不引入 `gix-worktree`(restore 通过 VFS 完成)。

---

# 6. 核心 Trait 设计
## 6.1 ObjectStore
`ObjectStore` 是 Git 内容寻址存储的抽象,提供 blob/tree/commit 三类对象的存取。所有写入按 SHA-1 内容寻址,天然幂等(同样的字节 → 同样的 oid)。trait 必须 `Send + Sync + 'static`,以便在 Tokio 多线程运行时中跨任务共享。
```rust
// crates/ragfs/src/git/object_store.rs
use async_trait::async_trait;
use bytes::Bytes;
use gix_hash::ObjectId;

/// 内容寻址的 Git 对象存储抽象
/// put 必须幂等;get 不存在返回 NotFound;exists 不读取内容
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    /// 写入一个已 zlib 压缩的 loose object
    /// oid 必须等于 SHA-1(未压缩 header + payload)
    async fn put(
        &self,
        account: &str,
        oid: &ObjectId,
        zlib_body: Bytes,
    ) -> Result<(), ObjectStoreError>;

    /// 读取并 zlib 解压(返回 header + payload 的原始字节)
    async fn get(
        &self,
        account: &str,
        oid: &ObjectId,
    ) -> Result;

    /// 仅检查存在性(HEAD/stat 优化,跳过内容传输)
    async fn exists(
        &self,
        account: &str,
        oid: &ObjectId,
    ) -> Result;
}

#[derive(Debug, thiserror::Error)]
pub enum ObjectStoreError {
    #[error("object not found: {0}")]
    NotFound(ObjectId),
    #[error("backend io: {0}")]
    Io(#[from] std::io::Error),
    #[error("zlib decode: {0}")]
    Zlib(String),
    #[error("oid mismatch: expected {expected}, got {actual}")]
    OidMismatch { expected: ObjectId, actual: ObjectId },
    #[error("backend error: {0}")]
    Backend(String),
}
```

ℹ️ **说明:** 物理路径布局由各实现自行决定(local 走 fanout 目录,s3 走 key prefix),trait 层不暴露物理路径,只暴露逻辑寻址。

## 6.2 RefStore
`RefStore` 是分支/标签的命名引用存储,核心是 **CAS(Compare-And-Swap)** 更新原语 — 这是 Git 一致性的基石。CAS 保证"两个并发 commit 先到先得,后到的看到 `Conflict` 并需要重试或 rebase",避免静默覆盖。
```rust
// crates/ragfs/src/git/ref_store.rs
use async_trait::async_trait;
use gix_hash::ObjectId;

#[async_trait]
pub trait RefStore: Send + Sync + 'static {
    /// 读取 ref 的当前值;不存在返回 NotFound
    async fn read(
        &self,
        account: &str,
        ref_name: &str,
    ) -> Result;

    /// Compare-And-Swap 更新:仅当当前值 == expected 时才写入 new
    /// expected = None 表示"仅当 ref 不存在时创建"
    async fn cas_update(
        &self,
        account: &str,
        ref_name: &str,
        expected: Option,
        new: ObjectId,
    ) -> Result<(), RefStoreError>;

    /// 列出 account 下的所有 refs(用于 log / branch 列表)
    async fn list(
        &self,
        account: &str,
        prefix: &str,
    ) -> Result, RefStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum RefStoreError {
    #[error("ref not found: {0}")]
    NotFound(String),
    #[error("CAS conflict: expected {expected:?}, actual {actual:?}")]
    Conflict {
        expected: Option,
        actual: Option,
    },
    #[error("invalid ref name: {0}")]
    InvalidName(String),
    #[error("backend io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend: {0}")]
    Backend(String),
}
```

⚠️ **注意:** ref 名必须经 `gix_validate::reference::name(...)` 校验,拒绝 `..`、空字符、特殊保留字等,避免路径穿越和注入。

## 6.3 命名约定

| 类别          | 路径模板                                    | 说明                                                       |
| ----------- | --------------------------------------- | -------------------------------------------------------- |
| Object      | `{root}/{account}/objects/{aa}/{bb...}` | Git 标准 fanout(前 2 hex 为目录，后 38 hex 为文件名)，便于分布式存储 list 优化 |
| Ref (heads) | `{root}/{account}/refs/heads/{branch}`  | 文件内容 = 40 hex 字符 + `\n`                                  |
| HEAD        | `{root}/{account}/HEAD`                 | 内容 = `ref: refs/heads/main\n`                            |
| Packed-refs | (不实现)                                   | MVP 全部 loose，后续如 ref 数量爆炸再补 pack                         |

***


# 7. 后端实现
## 7.1 LocalObjectStore / LocalRefStore
**LocalObjectStore** 直接持有 `plugins::localfs::LocalFileSystem` 实例(不经 MountableFS),把 Git 对象写入本地磁盘的 `{base_dir}/{account}/objects/{aa}/{bb...}`。**LocalRefStore** 用 `rename(2)` 原子重命名 + `flock` + 进程内 `tokio::sync::Mutex` 实现 CAS,覆盖跨进程与同进程两层并发。
```rust
// crates/ragfs/src/git/backends/local.rs (节选)
pub struct LocalObjectStore {
    fs: Arc,
    base_dir: PathBuf,            // e.g. /data/openviking/git
}

#[async_trait]
impl ObjectStore for LocalObjectStore {
    async fn put(&self, account: &str, oid: &ObjectId, body: Bytes) -> Result<()> {
        let hex = oid.to_hex().to_string();
        let path = self.base_dir
            .join(account).join("objects")
            .join(&hex[..2]).join(&hex[2..]);
        // 内容寻址 → 已存在则跳过(幂等)
        if tokio::fs::try_exists(&path).await? { return Ok(()); }
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        // 写临时文件 + rename 保证原子性
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, &body).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
    // get / exists 略
}

pub struct LocalRefStore {
    base_dir: PathBuf,
    // 进程内串行化 CAS,key = (account, ref_name)
    locks: dashmap::DashMap<(String, String), Arc>>,
}

#[async_trait]
impl RefStore for LocalRefStore {
    async fn cas_update(
        &self,
        account: &str,
        name: &str,
        expected: Option,
        new: ObjectId,
    ) -> Result<()> {
        gix_validate::reference::name(name.into())?;
        let lock = self.locks
            .entry((account.into(), name.into()))
            .or_default().clone();
        let _guard = lock.lock().await;
        let path = self.ref_path(account, name);
        let actual = read_ref_opt(&path).await?;
        if actual != expected {
            return Err(RefStoreError::Conflict { expected, actual });
        }
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, format!("{}\n", new.to_hex())).await?;
        // rename + fsync 父目录保证 crash-consistency
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}
```

## 7.2 S3ObjectStore / S3RefStore
**S3ObjectStore** 复用 `plugins::s3fs::S3Client`,将 object 存为 `{prefix}/{account}/objects/{aa}/{bb...}`。由于内容寻址,`put` 用 `If-None-Match: *` 头实现幂等"仅首次写入"。**S3RefStore** 用 `If-Match: "{etag}"` 实现 CAS,通过先 HEAD 拿 etag、再 PUT 提交。
```rust
// crates/ragfs/src/git/backends/s3.rs (节选)
pub struct S3RefStore {
    s3: Arc,
    bucket: String,
    prefix: String,
    cas_mode: CasMode,    // Native | RedisLock(用于不支持 If-Match 的后端)
}

#[async_trait]
impl RefStore for S3RefStore {
    async fn cas_update(
        &self,
        account: &str,
        name: &str,
        expected: Option,
        new: ObjectId,
    ) -> Result<()> {
        let key = self.ref_key(account, name);
        match self.cas_mode {
            CasMode::Native => {
                // 1. HEAD 取当前 etag
                let head = self.s3.head_object(&self.bucket, &key).await;
                let (current_etag, current_oid) = match (head, expected) {
                    (Ok(h), Some(exp)) => {
                        let body = self.s3.get_object(&self.bucket, &key).await?;
                        let cur = parse_oid(&body)?;
                        if cur != exp {
                            return Err(RefStoreError::Conflict {
                                expected: Some(exp),
                                actual: Some(cur),
                            });
                        }
                        (Some(h.etag), Some(cur))
                    }
                    (Err(NotFound), None) => (None, None),
                    (Err(NotFound), Some(_)) => {
                        return Err(Conflict { expected, actual: None })
                    }
                    (Ok(h), None) => {
                        let body = self.s3.get_object(&self.bucket, &key).await?;
                        return Err(Conflict {
                            expected: None,
                            actual: Some(parse_oid(&body)?),
                        });
                    }
                };
                // 2. 条件 PUT
                let body = format!("{}\n", new.to_hex());
                let result = self.s3.put_object_conditional(
                    &self.bucket,
                    &key,
                    body.into_bytes(),
                    PutCondition::IfMatch(current_etag.unwrap_or_default()),
                ).await;
                map_precondition_failed(result, expected, current_oid)
            }
            CasMode::RedisLock => {
                // Redis 分布式锁 + GET-then-PUT
                let lock = self.redis_lock.acquire(&key).await?;
                let actual = read_ref_via_get(&self.s3, &self.bucket, &key).await?;
                if actual != expected {
                    return Err(Conflict { expected, actual });
                }
                self.s3.put_object(&self.bucket, &key,
                    format!("{}\n", new.to_hex()).into_bytes()).await?;
                drop(lock);
                Ok(())
            }
        }
    }
}
```

⚠️ **S3 CAS 兼容性提示:** AWS S3 自 2024 年起支持 `If-Match` / `If-None-Match` 条件写;TOS / OSS 实现情况需在选型时验证。若某后端不支持原生 CAS,需退化为"分布式锁 + GET-then-PUT"模式,在 `S3RefStore` 构造时通过 feature flag 切换。

---

# 8. GitService 主流程
## 8.1 commit 完整实现
commit 主流程:**枚举 → 读 blob → 构建 tree → 构建 commit → CAS 更新 ref**。所有 ObjectStore 写入并发,所有写入完成后才更新 ref(避免悬空引用)。tree 未变 → 不创建空 commit(no-op 优化)。绝大多数 commit 场景下，被调用方声明为 "改动" 的文件里仍有大量未真正修改，需要通过三级 fast path 层层过滤，保证只有真正变化的字节才进入 streaming hash 与 blob 写入。

      层级

      触发条件

      节省的开销

      实现位置

      **Fast Path 1**: Stat 索引复用 oid

      文件 (size, mtime_ns) 与 prev_index 完全一致

      跳过 `vfs.read` + sha1 hash

      `commit_index.bin` per (account, branch), 复用 gix-index 格式

      **Fast Path 2**: Tree 子树原样保留

      子树下所有路径都没 upsert/remove

      跳过子树重 hash + 新 tree object 写入

      `gix-object::tree::Editor::write` 内部已实现

      **Fast Path 3**: Blob CAS 去重

      算出的 oid 在 object_store 已存在

      跳过 zlib 压缩 + put_blob (本地写盘 / S3 PUT)

      `object_store.exists(oid)`: 本地 `stat`,S3 `HEAD`

```rust
// crates/ragfs/src/git/service.rs ::commit (节选)
pub async fn commit(&self, req: CommitRequest) -> Result {
    let CommitRequest {
        account, branch, message, paths, author_name, author_email,
    } = req;

    // 0. 加载上次 commit 的 index(path -> (size, mtime, oid))与 HEAD tree
    let prev_index = self.load_commit_index(&account, &branch).await?;
    let prev_head  = self.resolve_ref(&account, &branch).await.ok();
    let prev_tree  = match prev_head {
        Some(oid) => self.load_commit(&account, &oid).await?.tree,
        None      => ObjectId::empty_tree(),
    };

    // 1. 用 gix-object Tree::Editor 在 prev_tree 之上做增量编辑
    let mut editor = tree_builder::Editor::from_tree(
        &self.object_store, &account, prev_tree,
    ).await?;
    let mut new_index = prev_index.clone();
    let mut changed = 0usize;

    // 2. 枚举候选路径:paths=Some → 显式清单;paths=None → 全量遍历 VFS
    let candidates = match &paths {
        Some(ps) => ps.clone(),
        None     => enumerate::collect_all(&self.vfs, &account).await?,
    };

    for path in candidates {
        let stat = match self.vfs.stat(&account_path(&account, &path)).await {
            Ok(s)  => s,
            Err(e) if is_not_found(&e) => {
                // 文件被删 → 从 tree 中移除,index 同步删除
                if prev_index.contains_key(&path) {
                    editor.remove(&path)?;
                    new_index.remove(&path);
                    changed += 1;
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        // ---- Fast Path 1: stat 一致 → 复用 prev oid,完全不读文件 ----
        if let Some(prev) = prev_index.get(&path) {
            if prev.size == stat.size && prev.mtime_ns == stat.mtime_ns {
                continue;  // editor 里已是 prev.oid,无需任何操作
            }
        }

        // ---- Fast Path 3: 必须读全量 + streaming hash,但可能跳过写入 ----
        let bytes = self.vfs.read(&account_path(&account, &path)).await?;
        let oid   = sha1_blob_streaming(&bytes);  // 流式 hash,内存 = 单 chunk

        if !self.object_store.exists(&account, &oid).await? {
            // 本地:stat objects/{oid[:2]}/{oid[2:]};S3:HEAD 同 key
            self.object_store.put_blob(&account, &oid, &bytes).await?;
        }
        editor.upsert(&path, oid)?;
        new_index.insert(path.clone(), IndexEntry {
            size: stat.size, mtime_ns: stat.mtime_ns, oid,
        });
        changed += 1;
    }

    // 3. 无任何变化 → 直接 noop 返回,不写新 commit、不动 ref
    if changed == 0 {
        return Ok(CommitResponse::Noop { commit_oid: prev_head.unwrap_or_default() });
    }

    // ---- Fast Path 2: editor.write() 内部自动复用未触碰子树的 tree_oid ----
    let new_tree = editor.write(&self.object_store, &account).await?;

    // 4. 构造 commit object → 写入
    let commit = CommitObject {
        tree: new_tree,
        parents: prev_head.into_iter().collect(),
        author: Actor::now(&author_name, &author_email),
        committer: Actor::now(&author_name, &author_email),
        message: message.into(),
    };
    let commit_oid = self.object_store.put_commit(&account, &commit).await?;

    // 5. CAS 更新 ref(local: rename+flock;S3: If-Match);失败 → ConcurrentCommit
    self.ref_store.cas_update(
        &account, &format!("refs/heads/{}", branch),
        prev_head, commit_oid,
    ).await?;

    // 6. 持久化新 index(下一次 commit 的 Fast Path 1 基础)
    self.save_commit_index(&account, &branch, &new_index).await?;

    Ok(CommitResponse::Created { commit_oid, changed })
}
```

## 8.2 restore 完整实现
restore 主流程:**解析目标 commit → 提取该 commit 中 project_dir 子树 → 与当前 HEAD 中同路径子树 diff → 通过 **`viking_fs.write/rm`** 回写 → 触发向量索引重建 →以当前 HEAD 为 parent 生成新 commit → CAS 更新 ref**。`dry_run` 模式只计算差异不写，用于预检。
关键差异：与 git checkout 不同，本接口**不移动分支指针到旧 commit**, 而是把 "旧内容" 作为新 commit 的工作树内容，正向写入。新 commit 的 parent 是当前 HEAD, 不是目标 commit, 这保证了：(1) HEAD 单调前进；(2) 非 `project_dir` 路径自动保留 HEAD 的最新内容，无需特殊处理；(3) restore 本身可以被再次 restore (因为它就是一个普通 commit)。
```rust
pub async fn restore(&self, req: RestoreRequest) -> Result {
    let RestoreRequest {
        account, branch, project_dir, source_commit, dry_run, message,
        author_name, author_email,
    } = req;

    // 1. 解析两端 commit
    let source_oid = self.resolve_ref(&account, &source_commit).await?;
    let source     = self.load_commit(&account, &source_oid).await?;
    let head_oid   = self.resolve_ref(&account, &branch).await?;  // 必须已有 HEAD
    let head       = self.load_commit(&account, &head_oid).await?;

    // 2. 在两棵 tree 中分别"截取" project_dir 子树
    //    source 没有该子目录 → 视为空树(等价于把整个目录删掉)
    let source_subtree = tree_builder::subtree(
        &self.object_store, &account, source.tree, &project_dir,
    ).await?.unwrap_or(ObjectId::empty_tree());
    let head_subtree   = tree_builder::subtree(
        &self.object_store, &account, head.tree, &project_dir,
    ).await?.unwrap_or(ObjectId::empty_tree());

    // 3. 子树之间 diff,得到三类操作(只限 project_dir 范围内)
    let target_entries  = tree_builder::flatten_tree(
        &self.object_store, &account, source_subtree,
    ).await?;
    let current_entries = tree_builder::flatten_tree(
        &self.object_store, &account, head_subtree,
    ).await?;
    let diff = compute_diff(&target_entries, &current_entries);

    if dry_run {
        return Ok(RestoreResponse::dry_run_from(diff, source_oid, head_oid));
    }
    if diff.is_empty() {
        // 工作区已经和 source 的 project_dir 一致,不产生新 commit
        return Ok(RestoreResponse::Noop {
            head: head_oid, source: source_oid,
        });
    }

    // 4. 并发回写 VFS:路径要带上 project_dir 前缀
    //    走完整 viking_fs.write/rm,触发现有 lock、加密
    let prefixed = |p: &str| format!("{}/{}", project_dir.trim_end_matches('/'), p);
    let written = stream::iter(diff.to_write)
        .map(|(path, blob_oid)| async move {
            let body = self.read_blob(&account, &blob_oid).await?;
            self.vfs.write(
                &account_path(&account, &prefixed(&path)), body,
            ).await?;
            Ok::<_, GitError>(prefixed(&path))
        })
        .buffer_unordered(32)
        .try_collect::>()
        .await?;
    let deleted = stream::iter(diff.to_delete)
        .map(|path| {
            let p = prefixed(&path);
            async move {
                self.vfs.rm(&account_path(&account, &p)).await?;
                Ok::<_, GitError>(p)
            }
        })
        .buffer_unordered(32)
        .try_collect::>()
        .await?;

    // 5. 在 head.tree 之上做增量编辑:
    //    把 project_dir 子树整体替换为 source_subtree。
    //    非 project_dir 路径原样保留 head 中的 tree_oid。
    let mut editor = tree_builder::Editor::from_tree(
        &self.object_store, &account, head.tree,
    ).await?;
    editor.upsert_subtree(&project_dir, source_subtree)?;
    let new_tree = editor.write(&self.object_store, &account).await?;

    // 6. 构造新 commit:parent = 当前 HEAD(不是 source_oid!)
    let commit = CommitObject {
        tree: new_tree,
        parents: vec![head_oid],                       // ← 关键:HEAD 单向前进
        author: Actor::now(&author_name, &author_email),
        committer: Actor::now(&author_name, &author_email),
        message: message.unwrap_or_else(|| format!(
            "restore {} from {}", project_dir, &source_oid.to_hex()[..12],
        )).into(),
    };
    let new_commit_oid = self.object_store.put_commit(&account, &commit).await?;

    // 7. CAS 更新 ref:expect=head_oid, new=new_commit_oid
    //    若期间有别的 commit 进入 → ConcurrentCommit,调用方按提示重试
    self.ref_store.cas_update(
        &account, &format!("refs/heads/{}", branch),
        Some(head_oid), new_commit_oid,
    ).await?;

    // 8. 同步刷新 commit_index(下次 commit 的 Fast Path 1 基础)
    self.refresh_commit_index_after_restore(
        &account, &branch, &written, &deleted,
    ).await?;

    Ok(RestoreResponse::Applied {
        new_commit_oid,
        source_commit: source_oid,
        parent_commit: head_oid,
        written: written.len(),
        deleted: deleted.len(),
        unchanged: diff.unchanged.len(),
    })
}
```

✅ **推荐:** 生产环境调用前先以 `dry_run=true` 跑一遍取得差异列表,再让用户确认,避免误覆盖未提交的本地变更。

## 8.3 show 完整实现
show 是**纯读路径**,无任何 VFS 写入或 ref 变更,易于实现与验证。支持两种模式:`path=None` 返回 commit 元信息(用于 log 列表);`path=Some(p)` 返回该 path 的 blob 字节。
```rust
// crates/ragfs/src/git/service.rs ::show (节选)
pub async fn show(&self, req: ShowRequest) -> Result {
    let ShowRequest { account, target_ref, path } = req;

    // 1. ref(可以是 branch / tag / 40-hex commit oid) → commit_oid
    let commit_oid = self.resolve_ref(&account, &target_ref).await?;
    let commit = self.load_commit(&account, &commit_oid).await?;

    match path {
        // 模式 A:返回 commit 元信息(log 用)
        None => Ok(ShowResponse::Commit {
            oid:       commit_oid,
            tree:      commit.tree,
            parents:   commit.parents,
            author:    commit.author.into(),
            committer: commit.committer.into(),
            message:   commit.message.to_string(),
        }),

        // 模式 B:返回该 path 的 blob 字节
        Some(p) => {
            // 按 / 拆分,在 tree 上逐层递归找到 blob_oid
            let blob_oid = tree_builder::lookup(
                &self.object_store, &account, commit.tree, &p,
            ).await?
              .ok_or(GitError::PathNotFound(p.clone()))?;

            let blob = self.load_blob(&account, &blob_oid).await?;
            Ok(ShowResponse::Blob {
                oid:   blob_oid,
                size:  blob.len() as u64,
                bytes: blob,
            })
        }
    }
}

/// resolve_ref 支持 3 种输入:
///   1. 40-hex commit_oid      → 直接解析
///   2. branch 名(如 "main")  → 加前缀 refs/heads/{branch}
///   3. 全路径 refs/heads/xxx  → 透传
fn resolve_ref(/* ... */) -> Result { /* ... */ }
```

---

# 9. Python Binding 与 VikingFS 集成
## 9.1 PyO3 binding 新增方法
在现有 `RAGFSBindingClient`(`crates/ragfs-python/src/lib.rs`)上追加三个 `#[pymethods]`。模式与 `ls/read/write` 一致:用 `py_detach_blocking` 释放 GIL,在 Tokio runtime 内调 `GitService`,返回结果序列化为 `PyDict`。
```rust
// crates/ragfs-python/src/lib.rs (追加)
#[pymethods]
impl RAGFSBindingClient {
    /// 提交一次快照
    /// kwargs: account, branch, message, paths(Option>),
    ///         author_name, author_email
    /// returns: {"commit_oid": str, "result": "created" | "noop"}
    fn git_commit(&self, py: Python<'_>, kwargs: &PyDict) -> PyResult {
        let req = parse_commit_request(kwargs)?;
        let svc = self.git_service()?;     // FeatureDisabled 时返回 PyErr
        py_detach_blocking(py, || {
            self.runtime.block_on(svc.commit(req))
                .map_err(map_git_error)
        }).map(|r| commit_response_to_pydict(py, r))
    }

    /// 定向恢复某个 project 目录,正向生成新 commit
    /// kwargs: account, branch(默认 "main"), project_dir, source_commit,
    ///         dry_run(bool=false), message(Option),
    ///         author_name, author_email
    /// returns:
    ///   Applied: {"new_commit_oid": str, "source_commit": str, "parent_commit": str,
    ///             "written": int, "deleted": int, "unchanged": int}
    ///   Noop:    {"noop": true, "head": str, "source": str}
    ///   DryRun:  {"dry_run": true, "diff": {...}, "head": str, "source": str}
    fn git_restore(&self, py: Python<'_>, kwargs: &PyDict) -> PyResult {
        let req = parse_restore_request(kwargs)?;
        let svc = self.git_service()?;
        py_detach_blocking(py, || self.runtime.block_on(svc.restore(req))
            .map_err(map_git_error))
            .map(|r| restore_response_to_pydict(py, r))
    }

    /// 读取 ref / commit / blob
    /// kwargs: account, target_ref, path(Option)
    /// returns:
    ///   path=None: {"oid","tree","parents","author","committer","message"}
    ///   path=str:  {"oid","size","bytes": PyBytes}
    fn git_show(&self, py: Python<'_>, kwargs: &PyDict) -> PyResult {
        let req = parse_show_request(kwargs)?;
        let svc = self.git_service()?;
        py_detach_blocking(py, || {
            self.runtime.block_on(svc.show(req))
                .map_err(map_git_error)
        }).map(|r| show_response_to_pydict(py, r))
    }
}

/// GitError → Python 异常映射(在 openviking 侧定义对应异常类)
fn map_git_error(e: GitError) -> PyErr {
    match e {
        GitError::FeatureDisabled    => PyRuntimeError::new_err("git feature disabled"),
        GitError::ConcurrentCommit   => PyValueError::new_err("concurrent commit conflict"),
        GitError::PathNotFound(p)    => PyFileNotFoundError::new_err(p),
        GitError::RefNotFound(r)     => PyFileNotFoundError::new_err(r),
        other                        => PyRuntimeError::new_err(other.to_string()),
    }
}
```

## 9.2 Python 侧 VikingFS 新增方法
在 `openviking/openviking/storage/viking_fs.py` 的 `VikingFS` 类上追加 4 个公开方法。Python 调用方使用 `viking://` URI,内部经 `_uri_to_tree_path` 转换为账号内 tree 路径后再传给 binding。
```python
# openviking/storage/viking_fs.py (追加)
class VikingFS:
    # 已有: read / write / rm / ls / mv / mkdir ...

    async def commit(
        self,
        *,
        message: str,
        paths: list[str] | None = None,        # viking://... URIs
        branch: str = "main",
        author_name: str | None = None,
        author_email: str | None = None,
    ) -> dict:
        """提交一次跨 scope 快照。返回 {commit_oid, result}."""
        account = self._current_account()
        tree_paths = [self._uri_to_tree_path(p) for p in (paths or [])]
        return await self._async_client.run(
            "git_commit",
            account=account,
            branch=branch,
            message=message,
            paths=tree_paths or None,
            author_name=author_name or self._default_author_name(),
            author_email=author_email or self._default_author_email(),
        )

    async def restore(
        self,
        *,
        project_dir: str,                    # viking://resources/proj_a/ 或 "resources/proj_a"
        source_commit: str,                  # 40-hex / branch / tag
        branch: str = "main",
        dry_run: bool = False,
        message: str | None = None,
        author_name: str | None = None,
        author_email: str | None = None,
    ) -> dict:
        """将 project_dir 恢复到 source_commit 状态,生成一个新 commit。

        语义等价于 git restore --source= --worktree --staged
        / && git commit。HEAD 单调前进,不会回退。
        """
        account = self._current_account()
        tree_dir = self._uri_to_tree_path(project_dir).rstrip("/")
        result = await self._async_client.run(
            "git_restore",
            account=account, branch=branch,
            project_dir=tree_dir, source_commit=source_commit,
            dry_run=dry_run, message=message,
            author_name=author_name or self._default_author_name(),
            author_email=author_email or self._default_author_email(),
        )
        if dry_run or result.get("noop"):
            return result

        # 增量向量更新:只对受影响的源文件,逐个 vectors_only 重算。
        # L0/L1 派生文件已随源文件一起从 git 回写到 VFS,不需要重新生成。
        from openviking.service.reindex_executor import ReindexExecutor
        executor = ReindexExecutor()
        ctx = self._current_request_context()
        for affected_path in result.get("affected_files", []):
            affected_uri = self._tree_path_to_uri(affected_path)
            if self._is_derived_file(affected_uri):
                continue
            asyncio.create_task(executor.execute(
                uri=affected_uri, mode="vectors_only",
                wait=False, ctx=ctx,
            ))
        return result

    async def show(
        self,
        target_ref: str,
        *,
        path: str | None = None,
    ) -> dict | bytes:
        """path=None → commit 元信息;path=str → blob 字节。"""
        account = self._current_account()
        tree_path = self._uri_to_tree_path(path) if path else None
        resp = await self._async_client.run(
            "git_show",
            account=account,
            target_ref=target_ref,
            path=tree_path,
        )
        if "bytes" in resp:
            return resp["bytes"]
        return resp

    async def log(
        self,
        *,
        branch: str = "main",
        limit: int = 20,
    ) -> list[dict]:
        """便捷封装:沿 parent 链反向遍历 commit。"""
        account = self._current_account()
        head = await self._async_client.run(
            "git_show", account=account, target_ref=branch, path=None,
        )
        result, current = [head], head.get("parents", [])
        while current and len(result) < limit:
            parent_oid = current[0]
            commit = await self._async_client.run(
                "git_show", account=account, target_ref=parent_oid, path=None,
            )
            result.append(commit)
            current = commit.get("parents", [])
        return result

    # --- 工具方法 ---
    def _uri_to_tree_path(self, uri: str) -> str:
        """viking://resources/a.md → 'resources/a.md'
        (去掉 viking:// 前缀,保留 scope 段作为 tree 一级目录)"""
        parsed = VikingURI.parse(uri)
        if parsed.scope in INTERNAL_SCOPES:
            raise ValueError(f"internal scope not versioned: {parsed.scope}")
        return f"{parsed.scope}/{parsed.relative_path}"

    async def _trigger_vector_rebuild(
        self, account: str, paths: list[str]
    ) -> None:
        """restore 后异步触发向量索引重建。
        实现可对接现有的 watcher / 任务队列;失败不影响 restore 结果。"""
        try:
            await self._vector_service.rebuild(account, paths)
        except Exception:
            logger.exception("vector rebuild failed for %s", account)
```

---

# 10. 配置规范
## 10.1 与 resources 对称的配置布局
配置位于现有 RAGFS 配置文件的 `[git]` 段,布局与 `[plugins.localfs_resources]` / `[plugins.s3fs_resources]` 完全对称,便于运维心智复用。`enabled = false` 时 binding 方法返回 `FeatureDisabled`,不影响现有 VFS。
```toml
# ragfs.toml 新增 [git] 段
[git]
enabled        = true
backend        = "local"          # "local" | "s3"
default_branch = "main"
author_name    = "openviking-bot" # commit 默认作者
author_email   = "bot@openviking.local"

# 本地后端
[git.local]
base_dir = "/data/openviking/git" # objects/refs 存储根
fsync    = "data"                 # "off" | "data" | "data+meta"

# 远程后端(与 plugins.s3fs_resources 配置同构)
[git.s3]
bucket            = "openviking-prod"
prefix            = "git"          # 全部 key = {prefix}/{account}/...
region            = "us-east-1"
endpoint          = "https://s3.amazonaws.com"
access_key_env    = "OV_S3_AK"     # 从环境变量读
secret_key_env    = "OV_S3_SK"
cas_mode          = "native"       # "native"(If-Match) | "redis_lock"
redis_lock_url    = ""             # cas_mode=redis_lock 时必填

# 高级调优
[git.tuning]
upload_concurrency   = 64          # commit blob 上传 buffer_unordered 并发度
restore_concurrency = 32          # restore 回写 VFS 并发度
ref_cas_max_retry    = 3
ref_cas_backoff_ms   = 50          # 指数退避基数
```

## 10.2 切换本地↔远程


| 维度         | local → s3 改动                                         |
| ---------- | ----------------------------------------------------- |
| 配置文件       | `backend = "local"` → `backend = "s3"`；填 `[git.s3]` 块 |
| Service 代码 | 无                                                     |
| Python 调用方 | 无                                                     |
| 数据迁移       | 一次性脚本:本地 `{base_dir}` 全量上传至 S3 key prefix(保持目录结构)     |

💡 从本地切到远程的全部成本 = 修改 `backend = "local"` → `backend = "s3"` + 填 `[git.s3]` 块。Service 代码、Python 调用方完全无感。这与 resources 目录"`plugins.localfs_resources` ↔ `plugins.s3fs_resources`"的切换体验完全对称。

---

# 11. 并发与一致性
## 11.1 写并发模型
| 层次        | 并发原语                     | 说明                               |
| --------- | ------------------------ | -------------------------------- |
| Blob 上传   | `buffer_unordered(64)`   | 内容寻址，天然幂等；同 oid 多次 put 安全        |
| Tree 写入   | 串行(`Editor::write` 自底向上) | 同 oid 幂等，但顺序必须自底向上               |
| Commit 写入 | 串行，最后一步                  | 同 oid 幂等                         |
| Ref 更新    | CAS                      | 本地: 进程锁 + rename(2)；S3: If-Match |

## 11.2 并发冲突处理

两个并发 commit 的时序:

```
Commit A: read ref → None → build tree → put objects → cas_update(None, A) → SUCCESS
Commit B: read ref → None → build tree → put objects → cas_update(None, B) → Conflict!
```

Commit B 收到 Conflict 后，可选择:

1. **重试**: 重新读取 ref 拿到 A，在 A 之上 rebase 本次变更，再次提交
2. **放弃**: 返回错误给用户

## 11.3 重试策略
- **幂等部分(blob/tree/commit 写)**: 单点重试 3 次,指数退避 (100ms / 400ms / 1.6s)
- **CAS 冲突**: 由 `GitService::commit` 内部最多重试 3 次(自动 re-read parent + 重建 tree + 重新 CAS);超过后返回 `ConcurrentCommit` 给 Python 层,由调用方决定
- **跨账号**: 不同 account_id 的 ref 路径不同,天然无冲突,可完全并行
---

# 12. 安全与隔离
## 12.1 账号隔离
- Git 数据路径全部以 `{account_id}` 为顶层前缀,与现有 `/local/{account_id}/` 隔离模型完全一致
- `GitService` 所有方法的第一个参数都是 `account_id`,binding 层从 `RequestContext.account_id` 注入,不允许跨账号访问
- Path 解析时必须经过 `validate_account_id`(白名单字符集 + 长度),防止 `../` 注入
## 12.2 加密
**重要:** 现有 `viking_fs.write` 在写入前会调 `_encrypt_content`。**commit 时不应再次加密**——blob 内容 = 当前 VFS 已加密内容,Git 是对密文做版本管理。restore 写回时走 `viking_fs.write`,会再次"加密"——这里需要绕过(或保持密文不变):restore 路径走 `MountableFS.write` 而非 `viking_fs.write`,避免双重加密;或为 `viking_fs.write` 增加 `raw=True` 参数,restore 调用时传入。

## 12.3 资源限制

| 维度           | 限制                                | 措施                         |
| ------------ | --------------------------------- | -------------------------- |
| 单 blob 大小    | ≤ 100MB                           | commit 前 stat 检查，超限报错      |
| 单 commit 文件数 | ≤ 50000                           | enumerate 阶段提前拒绝           |
| 账号 Git 容量    | 由 quota 系统单独管控                    | 放在 `[git.quota]`，首版默认 10GB |
| checkout 并发  | 同一 account\_id 同一时间仅 1 个 checkout | 进程内 Mutex，防止 VFS 写竞态       |

---

# 13. 错误处理
## 13.1 错误分类
## 13.2 Python 异常映射

| Rust Error                                              | Python Exception                | 语义          |
| ------------------------------------------------------- | ------------------------------- | ----------- |
| `InvalidAccountId`                                      | `AGFSInvalidPathError`          | 账号 ID 非法    |
| `RefNotFound` / `ObjectNotFound` / `PathNotFoundInTree` | `AGFSNotFoundError`             | 404 语义      |
| `ConcurrentCommit`                                      | `GitConcurrentCommitError` (新增) | 需要上层重试或人工介入 |
| `BlobTooLarge`                                          | `AGFSInvalidOperationError`     | 单文件超限       |
| `CorruptedObject`                                       | `AGFSInternalError`             | 底层数据腐烂，需要运维 |


---

# 14. 可观测性
## 14.1 Tracing/日志关键字段
- **span 名**: `git.commit`, `git.restore`, `git.show`
- **tag**: `account_id`, `branch`, `parent_oid`, `commit_oid`, `backend`
- **event**: `git.blob.put`(`oid`, `size`), `git.tree.write`, `git.ref.cas`(`expected`, `new`, `result`), `git.cas.conflict`
## 14.2 Metrics

| 指标                                 | 类型        | 维度                           |
| ---------------------------------- | --------- | ---------------------------- |
| `git_commit_total`                 | counter   | account\_id, branch, result  |
| `git_commit_duration_seconds`      | histogram | backend                      |
| `git_commit_files`                 | histogram | —                            |
| `git_commit_bytes`                 | histogram | backend                      |
| `git_cas_conflict_total`           | counter   | account\_id, branch          |
| `git_object_store_latency_seconds` | histogram | op (put/get/exists), backend |
| `git_ref_store_latency_seconds`    | histogram | op (read/cas), backend       |


## 14.3 健康检查
- RAGFSBindingClient 现有 `health()` 方法增加 `git` 字段:返回 `{"backend": "local", "writable": true, "last_commit_age_sec": 12}`
- 每分钟后台心跳:对 `refs/heads/main` 做一次 read,失败则标记 degraded
---

# 15. 测试策略
## 15.1 测试层次

| 层级               | 范围                                                                                                                  |
| ---------------- | ------------------------------------------------------------------------------------------------------------------- |
| **单元测试 (Rust)**  | ObjectStore 各操作的幂等性；RefStore CAS 在并发下的正确性 (loom 测试)；tree\_builder 的 upsert/remove/write；错误映射                        |
| **集成测试 (Rust)**  | LocalObjectStore vs MemObjectStore 跑同一组场景；commit → show 路径 → bytes 一致；commit → checkout → 文件一致；并发 commit 的 CAS 冲突处理 |
| **端到端 (Python)** | VikingFS.commit → checkout 全流程；跨 scope 原子快照；派生文件被正确纳入 commit 并随 checkout 回滚；向量索引在 checkout 后被重建；多账号并发隔离             |

## 15.2 关键测试用例清单
1. **幂等性**: 同一 commit_req 调用两次,第二次应快速返回(blob exists 跳过 + ref 未变 → no-op 或 same oid)
1. **跨 scope 原子性**: 一次 commit 同时改 `resources/a.md` 和 `agent/skills/b.py`,restore 父 commit 后两者都应回滚
3. **派生文件纳入**: 创建 `resources/x.md` 与 `resources/x.md.abstract.md`,commit 后 `show` 两者均可见;restore 父 commit 后两者都应回滚;向量索引文件不被 commit
1. **CAS 冲突**: 两个并发 commit,后到的必须看到 `ConcurrentCommit` 错误而非默默覆盖
1. **dry_run 不写**: restore dry_run 后再 ls,VFS 状态不变
1. **账号隔离**: A 账号的 commit_oid 在 B 账号下 show 必须返回 not found
1. **后端等价性**: LocalObjectStore 与 S3ObjectStore (LocalStack/MinIO) 跑同一组用例输出一致
1. **大文件**: 单 blob 80MB 可正确 commit / show / restore
1. **双重加密**: restore 写回后 VFS read 内容与原始明文一致
---

# 16. 实施计划 (MVP)

| 阶段 | 工作内容 | 交付物 | 预估 |
|------|----------|--------|------|
| D1-D2 | 新建 crates/ragfs/src/git/,定义 trait + LocalObjectStore/LocalRefStore + S3ObjectStore/S3RefStore (含 If-Match CAS) + 单测 | 裸 Git 存储跑通 put/get/CAS | 2d |
| D3 | 接入 gix_object::tree::Editor,实现 GitService::commit | commit 流程单测绿 | 1d |
| D4 | 实现 GitService::show (纯读路径,易验证) | commit + show 闭环 | 1d |
| D5 | 实现 GitService::restore,dry_run 优先,验证幂等 | commit + restore 闭环 | 1d |
| D6 | PyO3 binding: RAGFSBindingClient 三个新方法 + 错误映射 | Python 端可调 | 1d |
| D7 | VikingFS.commit/restore/show/log + URI ↔ tree path 转换 | Python 端到端 | 1d |
| D9 | tracing/metrics 接入 + health check | 可观测性完备 | 0.5d |
| D10 | 文档 + 灰度发布 | 上线 Phase 1 | 0.5d |

**总预估工时：8天**
---

# 18. 风险与缓解

      风险

      影响

      缓解

      S3/TOS CAS 兼容性差异

      高

      POC 阶段验证目标后端;不支持时切换 `cas_mode = "redis_lock"`

      大账号 commit 时 enumerate 慢

      中

      `paths` 参数限定 scope;后续引入增量 diff(基于 mtime + parent tree)

      双重加密导致 restore 后内容损坏

      高

      restore 路径绕过 `viking_fs.write` 加密,直接走 `MountableFS`;集成测试覆盖

      L0/L1 派生文件纳入版本历史,模型异步重建导致 commit 间差异增加

      中

      用户主动控制 commit 时机,不自动触发;L0/L1 文件通常较小(< 10KB),存储成本可控;如需降频可配置 commit 时忽略 mtime-only 变更

      同一账号多 Agent 高并发 commit

      中

      CAS 冲突自动重试 3 次;长期可引入"基于队列的串行化提交器"

      Git 数据无 GC,长期膨胀

      中

      首版不做 GC,运维侧定期 dump + 压缩;后续接入 reachability-based GC

      loose object 数量爆炸,本地 inode 紧张

      低

      Phase 4 引入 pack file;Git fanout 已经缓解一半

---

# 19. 后续演进方向
1. **Pack file 支持**: 引入 `gix-pack`,对历史 commit 做 delta 压缩,降低存储成本 80%+
1. **Auto-commit hook**: 在 `content_write.ContentWriteCoordinator` 末尾追加可选 hook,实现"每次写自动 commit"模式(Phase 2 重新评估)
1. **Branch / Tag 管理**: 暴露 `branch_create / branch_delete / tag` API
1. **Diff API**: `diff(ref_a, ref_b)` 返回结构化差异,供 UI 渲染
1. **跨账号镜像**: 支持账号间的 commit 分享(类似 GitHub fork)
1. **向量索引版本化(可选)**: 若后续需要向量索引的快照回滚能力,可引入轻量 manifest 记录 index 版本与对应 commit_oid 的映射,避免全量存储向量数据
1. **外部 Git 工具兼容**: 输出标准 Git 仓库格式,允许通过 `git clone file://...` 检视
---

# 20. 附录
## 20.1 术语表

      术语

      含义

      VFS

      Virtual File System,本文特指 OpenViking 的 `MountableFS` + plugin 体系

      Loose Object

      Git 的基础存储单元,zlib 压缩,按 SHA 寻址的单文件

      CAS

      Compare-And-Swap,本文特指 ref 更新时"仅当当前值 = 期望值才写入"

      Root Tree

      commit 对象指向的最顶层 tree 对象,代表整个仓库快照

      Tree Editor

      `gix_object::tree::Editor`,gitoxide 提供的内存中 tree 构建器,支持 upsert/remove/write

      派生文件

      `.abstract.md` / `.overview.md` / `.relations.json`,由 OpenViking 模型异步生成的 L0/L1 摘要文件,已纳入 Git 版本管理

## 20.2 参考资料
- [GitoxideLabs/gitoxide](https://github.com/GitoxideLabs/gitoxide)
- [volcengine/OpenViking](https://github.com/volcengine/OpenViking)
- [OpenViking 存储架构文档](https://github.com/volcengine/OpenViking/blob/main/docs/zh/concepts/05-storage.md)
- [Git Pack Format (后续 Phase 参考)](https://git-scm.com/docs/gitformat-pack)
**文档完成**。如需对某一章节细化(如某后端实现细节、某测试用例代码、迁移脚本),请告知具体目标。
