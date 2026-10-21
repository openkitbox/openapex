# ox-snap 设计：macOS 版按快照的 agent 工作区存储

> 状态：设计定稿（2026-10-21），待实现。
> 背景：ox 的罚单 agent 需要「每 agent 独立可写工作区 + 提交历史」。
> 参考 LayerFS 的架构思想（Store/Branch/Commit/CAS 发布/单写者短持锁），
> 不使用 LayerFS 本体（其受支持部署依赖 Linux + Docker，ox 在 macOS 原生运行）。

## 1. 目标与非目标

**目标**

- 每个 agent / 子会话一个独立可写工作区，mount 成本接近零（APFS clonefile CoW）；
- 工作区内容完整：含 `.git`、忽略文件、依赖目录、缓存——无 git 模型边界；
- Commit 产生不可变快照 id，可作为工单证据引用；
- 同一分支并发 commit 有确定语义：一方发布，另一方得到确切 `Conflict`；
- 单 crate、纯同步、公开面最小，供 ox（未来的多 crate workspace）调用。

**非目标（一期明确不做）**

- 三路自动 merge（冲突如实上报，处置策略留给调用方）；
- 分块去重 / delta 编码（文件级去重已覆盖依赖目录这类大头）；
- FUSE / 内核拦截（diff 式捕获对本场景足够）；
- 文件级历史索引（blame 类查询二期加侧表）；
- git 互操作导出（已评估，推迟：证据闭环一期走内部 `diff`，
  将来需要 PR 化时在 `FileChange` 列表上渲染 unified diff，约百行，非架构决策）；
- 远程同步、加密、Docker、守护进程、任何网络面。

## 2. 形态与位置

- crate 名 `ox-snap`（lib 名 `ox_snap`），位于 `crates/ox-snap/`；
- ox 根 `Cargo.toml` 转为 workspace：`members = [".", "crates/ox-snap"]`，ox-snap 是第一个成员；
- 纯同步 API；ox 侧调用走 `tokio::task::spawn_blocking`；
- 依赖上限 6 个：`rusqlite`(bundled)、`blake3`、`walkdir`、`thiserror`、`libc`（clonefile）、
  第六个仅在确有需求时引入；无 async、无 serde（manifest 手写规范二进制编码）。

## 3. 模块划分与公开面

```
crates/ox-snap/src/
  lib.rs        facade：仅 re-export 5 个公开类型 + Error，目标 <150 行
  store.rs      Store：打开/建库/初始导入/分支操作
  workspace.rs  Workspace：mount/unmount/注册/崩溃清扫
  commit.rs     diff → manifest → blob 入池 → CAS 发布
  manifest.rs   manifest 编解码与对比（纯函数，无 IO）
  error.rs      唯一 Error 枚举（thiserror）
```

公开 API（其余一律 `pub(crate)`）：

```rust
pub struct Store;       // 不透明句柄
pub struct Workspace;   // mount 句柄，记录 branch 与 base commit
pub struct CommitId(pub [u8; 32]);
pub struct SnapshotId(pub [u8; 32]);   // manifest hash
pub struct FileChange { /* Added/Modified/Deleted + path */ }

pub enum CommitOutcome {
    Published(CommitId),
    Conflict { head: CommitId },
}

impl Store {
    pub fn open(path: &Path) -> Result<Self>;
    pub fn import(path: &Path, source: &Path) -> Result<Self>;
    pub fn create_branch(&self, name: &str, from: Option<&str>) -> Result<()>;
    pub fn mount(&self, branch: &str, dir: &Path) -> Result<Workspace>;
    pub fn commit(&self, ws: &Workspace, msg: &str) -> Result<CommitOutcome>;
    pub fn unmount(&self, ws: Workspace) -> Result<()>;   // 消费句柄
    pub fn head(&self, branch: &str) -> Result<CommitId>;
    pub fn diff(&self, a: CommitId, b: CommitId) -> Result<Vec<FileChange>>;
}
```

边界纪律：不泄漏 `rusqlite`/`walkdir` 类型；错误单一 `Error` 枚举，带确切原因；
一次尝试、不重试（沿用 LayerFS 的 one-attempt 纪律）；调用方不依赖重试语义。

## 4. 存储格式

```
<store>/
  store.db            SQLite：WAL，synchronous=NORMAL（本地单内核，要真崩溃持久性）
  blobs/ab/cdef…      blake3 内容寻址 blob 池（前 2 位分桶）
  manifests/<hash>    manifest 规范二进制
  ws/<uuid>/          活跃工作区（物化树）
  writer.lock         专用锁文件（不在 store.db 上取 flock：
                      Darwin 的 flock 与 SQLite 打开文件锁互相干扰，见 LayerFS 源码记录）
```

SQLite schema（3 张表，无更多）：

```sql
CREATE TABLE commits (
  id BLOB PRIMARY KEY, branch TEXT NOT NULL, parent BLOB,
  manifest BLOB NOT NULL, message TEXT NOT NULL, ts INTEGER NOT NULL
);
CREATE TABLE branches (name TEXT PRIMARY KEY, head BLOB NOT NULL);
CREATE TABLE workspaces (
  id TEXT PRIMARY KEY, branch TEXT NOT NULL, base BLOB NOT NULL,
  path TEXT NOT NULL, pid INTEGER NOT NULL
);
```

manifest = 全树扁平条目表，规范编码 `[(path, mode, size, blake3)]`，
整体 hash 即 `SnapshotId`。一期不拆分层 tree（5 万条目 ≈ 8 MB 单 blob，可接受）。

## 5. Workspace 生命周期

**mount(branch, dir)**
1. 读 branch head → 取 manifest；
2. 逐条目 `clonefile(blob_path, dir/rel_path)`——CoW：物化元数据、共享数据块、零数据拷贝；
3. `workspaces` 表注册（uuid、branch、base、path、pid）。

**commit(ws, msg)**
1. 流式 walk 工作区，与 base manifest 按 `(path, size, mtime)` 比对；
   未变文件不读内容、不 hash；
2. 变化候选流式 blake3（`io::copy` 进 hasher，64 KB 缓冲，永不整文件入内存）；
3. 新 blob：`stat(pool/hash)` 命中即跳过（跨 commit/跨 branch 天然去重）；
   未命中则 `clonefile(工作区文件, pool/hash)`——不写一字节数据；
   CoW 语义保证后续工作区写入不污染池中 blob（替代 hardlink 的就地写风险）；
4. 编码新 manifest 入 `manifests/`；
5. CAS 发布：`flock(writer.lock)` 阻塞等待 → 校验 `head == base` →
   单事务写 commits 行 + 更新 branches.head → 放锁。
   head 已变则返回 `Conflict { head }`，不重试、不自动合并。

**unmount(ws)**：消费句柄 → 注销行 → 删除目录（APFS 秒删，不动历史）。

**崩溃清扫**：`Store::open` 时扫 `workspaces` 表，pid 已死的行连同目录删除。
已知边界：pid 复用可能误判，文档注明；误判代价是删一个活跃工作区目录，
可接受但不静默——删除时 log warn。

## 6. 并发与一致性规则

- 多进程/多线程共享一个 Store 文件：SQLite WAL 读者不挡写者；
- 写者串行：`writer.lock` 一条阻塞通道，重活在锁外（diff/hash/clonefile 全部在拿锁前完成），
  锁内只做 head 校验 + 单事务写入——照搬 LayerFS「短持锁」原则；
- 锁等待被信号打断（EINTR）返回确切 `Cancelled`，不重试；
- 读操作（head/diff）不拿 writer.lock。

## 7. 内存与拷贝控制

- hash、blob 入池全程流式，单文件常驻 O(64 KB)；
- manifest 编解码逐条目进行，不构造中间字符串树；
- API 出入参一律 `&Path`/`&str`/`&[u8]`，所有权数据仅在调用方确实需要处返回
  （`Vec<FileChange>`）；
- mount/commit 的开销大头是 syscall（clonefile/stat），不是堆分配；
  manifest 的 `Vec<Entry>` 若 profiling 证实为热点，二期改流式编码——一期先正确。

## 8. ox 接入边界（不属于本 crate）

ox-snap 是纯存储库，不含任何 agent/会话概念。ox 侧接线（另案实现）：

- session/subagent 启动：`mount` → session directory 指向工作区路径；
- 结束：可选 `commit` → `unmount`；
- 工单正文记录 `{branch, commit_id}` 作证据；
- 只读约束走 ox 计划中的 read_only 注册表（`.opencode/plans/ox-parallel-subagent.md`），
  不由 ox-snap 负责。

## 9. 测试策略

- 单元：manifest 编解码往返；`(path,size,mtime)` 对比矩阵；CAS 冲突路径；
- 集成（全部有界等待、无网络、无 Docker）：
  - import → mount → 编辑 → commit → diff 验证 → 再 mount 验证内容；
  - 同分支两 workspace 并发 commit：恰一方 `Published`、一方 `Conflict`；
  - kill -9 持有方后重开 Store：崩溃清扫删除残留；
  - blob 去重：两 branch 提交相同文件，池中只一份；
  - CoW 隔离：commit 后继续写工作区文件，池中 blob 内容不变。

## 10. 已决事项

| 事项 | 决定 | 日期 |
| --- | --- | --- |
| 方案选型 | A：APFS clonefile + 内容寻址池（否决 git bare repo：嵌套 .git 模型边界；否决 macFUSE：成本远超收益） | 2026-10-21 |
| 位置/命名 | `crates/ox-snap`，ox 转 workspace 的第一个成员 crate | 2026-10-21 |
| git 互操作 | 推迟；一期仅 `diff` | 2026-10-21 |
| 持久性 | WAL + synchronous=NORMAL（本地场景，不继承 LayerFS 的 Disposable 限制） | 2026-10-21 |
