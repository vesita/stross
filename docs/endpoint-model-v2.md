# 端点模型 v2：节点 → 端点 → 策略 三层注册 + 订阅驱动（设计规格）

> 状态：**已落地（单轮实施）**——三层统一注册表（本机 + 互联节点同一张表）、
> 策略组合（序列化规则 + pick 规则，`strategy()` 组合方法替代 v1 `pick_rule()`）、
> 分享端/订阅端双特性（`share` / `subscribe` + 订阅端点生成）、协商/订阅流程按
> `(节点, 端点, 策略)` 定位；v1 规格 [endpoint-model.md](endpoint-model.md) 已删除
> （历史见 git）。
> 核心：端点（可分享内容）= 双向能力体（既能被订阅、也能订阅别人）。统一注册表只记录
> **策略（序列化规则 + pick 规则）**，数据按策略 id 匹配管线，内核调度启动、
> 解序列化后转发，订阅端点自己解析。
> 关联：[comm-mode-v2.md](comm-mode-v2.md)（pick 规则）·
> [layering-architecture.md](layering-architecture.md)（分层铁律）。

本文档**弃用「设备」一词**（设备有歧义，易与端点混淆），统一使用两个精确概念：

- **节点**（手机/电脑这类实体，即「设备」）：手机、电脑这样的实体，是**上层**。
- **端点**（节点上可分享的内容）：屏幕/麦克风/系统声音/文件等可分享内容，是节点的**下属**。

层级关系：一个**节点**（上层）拥有**多个端点**（下属）——一对多。
**分享/订阅方向挂载在端点层**：端点既能被订阅（分享端）、也能主动订阅别人
（订阅端）。节点只是「拥有多个端点」的容器，**不承载方向**。

## 0. 数据管道模型

> **原始数据 → 装载(序列化) → 传输 → 接收 → 解读(pick) → 呈现数据**

```
数据包(带策略id) ──► 内核解析出策略
   ├─ 按策略匹配 序列化规则 + pick 规则 → 对应管线
   ├─ 管线未启动 → 内核调度启动该管线
   ├─ 解序列化后的数据 → 转移到订阅端点
   └─ 订阅端点自己解析出数据（呈现）
```

- **序列化规则**：数据 ↔ 管线格式的转换（装载/解装载，含分包），端点自定；
- **pick 规则**：管线内如何解读（严格顺序 / 严格即时），见 comm-mode-v2.md；
- **管线** = 序列化规则 + pick 规则的组合；内核按策略匹配并调度启动。

## 1. 心智模型（用户定稿）

> **节点 = 手机/电脑这样的实体；端点 = 这个节点上可分享的内容。**
> 端点向本地节点注册自己（序列化规则 + pick 规则）→ 内核维护统一注册表
> → 其它节点想订阅时，从注册表拿 `(节点 id, 端点 id, 策略 id) → 策略组合`
> → 订阅端点生成。
> 思路：节点(上层) → 本地注册(内容即端点) → 订阅分享注册表 → 订阅端点生成。
> **每个端点至少需要完成分享端、订阅端两个特性**，方向挂在端点层，
> 节点只是容器。

```
节点（上层，手机 / 电脑）       端点（下属，节点上可分享的内容）
  ├─ 屏幕                        ├─ 屏幕共享
  ├─ 麦克风                      ├─ 麦克风
  ├─ 系统声音                    ├─ 系统声音
  └─ 文件                        └─ 文件
```

**内核平台无关**：节点承担平台差异（获取规则/系统约束），内核只维护
注册表 + 读策略标记执行。

## 2. 统一注册表（三层嵌套：节点 → 端点 → 策略）

```rust
/// 统一注册表：互联节点 → 端点 → 策略。所有参与互联的节点（含本机）都在这一张表。
struct UnifiedRegistry {
    nodes: HashMap<NodeId /* 互联节点id, 如手机/电脑 */, NodeRegistration>,
}

/// 一个互联节点（手机/电脑）的注册：节点信息 + 它拥有的端点（可分享内容）。
struct NodeRegistration {
    info: NodeInfo,                       // name / is_self(是否本机) / addr
    endpoints: HashMap<EndpointId, EndpointRegistration>,  // 该互联节点的下属端点
}

/// 一个端点（节点上可分享的具体内容，如"屏幕"/"麦克风"）。
struct EndpointRegistration {
    kind: MediaKind,                      // 内容类型：屏幕/麦克风/系统声音/文件
    name: String,
    target: TargetKind,
    /// 端点自主声明的策略组合（策略独立可寻址，同一内容可有多种处理组合）。
    strategies: HashMap<StrategyId, EndpointStrategy>,
}

/// 策略：注册表只记录「这个数据包怎么处理」的两要素。
struct EndpointStrategy {
    serialize: SerializeRule,    // 序列化规则（装载/解装载，含分包）——端点自定
    pick: PickRule,              // 解读规则（严格顺序/严格即时）——comm-mode-v2
}
```

**关键特性**：
- **注册表 = 互联节点集合**：所有参与互联的节点（手机/电脑）都在这张表里，
  本机只是其中一个互联节点（`is_self=true` 便于 UI 高亮，非特殊身份）。
  订阅统一 `registry[节点][端点][策略]` 查表，自订与订其它互联节点走同一套逻辑；
- **策略独立可寻址**（`StrategyId`）：一个端点（内容）可有多个策略（同内容不同
  序列化/pick 组合），订阅时按 `(节点 id, 端点 id, 策略 id)` 精确取；
- 订阅生成：`registry[节点][端点][策略]` → 策略组合 → 生成订阅端点。

### 为什么注册表只存这两要素

- **序列化规则**：决定「怎么把数据转成管线格式」，包编解码/分包——端点自定，
  内核不碰编码细节；
- **pick 规则**：决定「管线里怎么解读」，严格顺序/严格即时；
- **传输档案不进注册表**：允许丢包/不允许丢包是传输层契约，由端点声明、
  传输模块执行，不属于「数据包怎么处理」的策略核心。

## 3. 分享端 / 订阅端双特性

端点（可分享内容）= 双向能力体，既能被订阅（分享端）、也能订阅别人（订阅端）。
**方向就挂载在这一层（端点层），不是节点层**：节点只是「拥有多个端点」
的容器，不承载方向。

```rust
trait Endpoint: Send + Sync {
    /// 分享端：被订阅后开推。
    fn share(self, app, ctx: SubscribeCtx);
    /// 订阅端：主动订阅别人并处理（端点作为宿主处理订阅流/数据）。
    fn subscribe(self, app, spec: SubscribeSpec);

    /// 端点自主声明的策略（序列化规则 + pick 规则）。
    fn strategy(&self) -> EndpointStrategy;
}
```

`subscribe` 让「屏幕端点作为宿主处理订阅流」「剪贴板端点订阅别人」成为可能。

## 4. 数据流（设计规格）

```
注册：
  端点 → 本机（作为互联节点之一）注册 { 序列化规则, pick 规则 } → UnifiedRegistry[本节点][端点][策略]
  其它互联节点 → 发现/目录拉取 → 映射进 UnifiedRegistry[该互联节点][端点][策略]

订阅：
  订阅方从 UnifiedRegistry[节点][端点][策略] 取策略组合
  → 协商授予 → 订阅端点生成

数据面（建链后自治）：
  发送方 原始数据 → 序列化规则(装载/分包) → 管线帧(带策略id)
  → 传输 → 接收方内核
  → 按策略id解析 序列化规则+pick规则 → 匹配管线
  → 管线未启动→调度启动
  → 解序列化数据 → 转移到订阅端点 → 订阅端点自己解析成呈现数据
```

## 5. 实施范围（已落地）

一步到位，无阶段拆分。核心改动（均已完成）：

- **proto**：新增 `EndpointStrategy { serialize, pick }`（`SerializeRule` 含
  分包策略，当前全部端点声明直通 Passthrough）、`SubscribeSpec`、
  `StrategyId`；`EndpointManifest.strategies` 策略组合列表（平铺
  `transport_profile`/`pick_rule` 保留为默认策略的协商摘要，wire 兼容旧对端）；
  `ShareRequest.strategy_id`（订阅方选定策略）+ `ShareGrant.strategy`（定稿组合）；
- **endpoint**：`Endpoint` 契约 v2——`strategy()` 组合方法（`serialize + pick`）
  替代 v1 的 `pick_rule()` 散方法；`transport_profile()` 保留（传输档案由端点
  声明、传输模块执行，不进注册表）；新增 `subscribe` + `supports_subscribe()`
  （订阅端；默认不支持）；`SubscribeCtx.strategy` 携带定稿策略组合；
  `EndpointApp.receive_file`（订阅端文件接收调度能力）；
- **kernel**：`UnifiedRegistry`（节点→端点→策略三层，本机 + 互联节点统一；
  目录拉取映射远端节点；`resolve_strategy` 统一查表；`generate_subscribe_endpoint`
  订阅端点生成）；订阅编排按 `(节点, 端点, 策略)` 解析策略组合构建
  `SubscribeSpec`；`subscriber` 委托端点 `subscribe`（文件订阅端
  `FileReceiveEndpoint` 落盘）；
- **discovery/negotiator**：按 `(节点, 端点, 策略)` 定位；方向在端点
  层（不挂到节点层）。

## 6. 未决项（实施时拍板）

| 项 | 待定 | 落地 |
|---|---|---|
| `SerializeRule` 形态 | 序列化规则是枚举还是 trait/协议标记 | **枚举**（确定性，wire 可比对）+ 端点实现映射；当前全部端点声明 `Passthrough`（直通），`Chunked`（分包）预留 |
| 策略 id 粒度 | 1:1（策略 id=端点 id）还是独立可复用 | **独立 `StrategyId`**（模型既定，支持多策略）；当前每端点一个默认策略（id=`default`），未知策略 id 解析返回 None |
| 管道管线归属 | 管线（序列化+pick 组合）建于哪层 | 内核按策略匹配并调度启动（订阅端点在其上解析） |
| 传输档案 | 是否仍由端点声明（不进注册表） | 保留端点声明、传输模块执行（注册表聚焦序列化+pick） |
| 订阅端宿主范围 | 哪些端点实现 subscribe | 文件端点（接收落盘）首个落地；媒体播放仍由内核接收链路 + 壳层承担（暂无订阅端点宿主，`generate_subscribe_endpoint` 返回 None） |

## 7. 收尾（已落地记录）

- 本文档状态 → 「已落地（单轮）」；
- `endpoint-model.md` v1 **已删除**（历史见 git；v2 为唯一规格源）；
- `iteration-plan.md` 记录轮次；`dev-playbook.md` 增补「三层注册表」
  「订阅端特性」坑位。

### 落地差异（实施时对蓝图的小修正，均符合总思路）

1. **`subscribe` 为 fire-and-forget**（与 `share` 同构，端点自驱动 spawn）；
   需要同步落盘结果的 CLI 路径（`subscribe_file`）仍由内核订阅编排返回
   `SubscribeOutcome`——两者共用同一握手 + `SubscribeSpec` 构建
   （`subscribe_file_via_endpoint` 为订阅端点生成框架路径，进程内闭环已测）。
2. **远端 `TargetKind` 不落 wire**：目录清单无 target 字段，按协商档案
   （Lossless + StrictOrdered → 确定目标，否则实时目标）推断。
3. **策略解析回退链**：注册表查表 → 授予 `strategy` → 平铺 `pick_rule` 推导
   直通 + pick 默认策略（旧对端兼容）。
4. **接收端点可用性**：`FileReceiveEndpoint` 不探测源可用性（落盘目录由
   接收时创建），`supports_subscribe()==true` 且 `share` 恒告警（不进通告/目录）。
