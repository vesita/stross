# 前端 UI 逻辑重构设计（纯前端）

> 目标：消除「设备 × 共享流」界面的结构性混乱，对齐项目定位
> （本机能力注册 → mDNS 发现对端 → 发现对端已推送的流 → 订阅其流），
> 不触碰 Rust 后端、不改 mDNS TXT 能力清单（留待后续阶段打通）。
>
> 范围：`apps/stross-gui/web/app/`。约束：保留全部 DOM 契约
> （`data-act` / class / id），`scripts/test-frontend.mjs` 41 项断言作
> 行为不回归护栏；`tsc strict` + `check-frontend.sh` 全绿。

## 1. 现状问题（诊断）

| # | 问题 | 表现 |
|---|---|---|
| P1 | 单文件职责爆炸 | `grid.ts` 1099 行混「设备扫描 / 渲染 / 广播弹窗 / 凭证协商 / 电脑端收麦克风 / 防火墙」五个域 |
| P2 | 状态源分散 | `state.ts` 集中了大部分，但 `recvStreamId`(watch)、`pendingApprove`(main)、`localStreamsCache`/`shareModalStarter`(grid) 散落在外 |
| P3 | 脆弱互斥拼真相 | `shareItems()` 靠 `!(micShare && micShare.active)` 区分广播/定向；出站由 `streaming`/`shareKind`/`streamInfo`/`micShare` 四个变量拼出 |
| P4 | 重复状态机 | 入站有 `startReceive`/`pollReceiveStatus` 与 `startMicReceive`/`pollMicRecv` 两套；出站有 `pollStatus`（send）与 `pollMicShareStatus`（grid）两套轮询 |
| P5 | 术语不统一 | 「点流即收」未显式叫「订阅」；设备卡片展示「角色」而非「能力」（后端能力模型已就绪，UI 未呈现） |

## 2. 目标信息架构（对齐用户定位）

```
① 本机（能力提供方）        ② 局域网设备（被发现的对端）       ③ 订阅（接收端主动权）
┌──────────────────┐     ┌──────────────────────────┐     ┌──────────────────────┐
│ 能力清单（可勾选） │     │ 设备卡片：名称 + 能力徽标    │     │ 运行面板（动态增删/画质）│
│ ☑ 屏幕 ☑ 麦克风   │     │  + 已推送的流列表           │     │ 点流即收（保留便捷）     │
│ ◻ 摄像头 ◻ 系统声  │     │  （点设备展开）             │     │ 建立后可改画质/断线重连  │
└──────────────────┘     └──────────────────────────┘     └──────────────────────┘
```

接收端「点流即收」保留（用户已确认），订阅面板承担**运行期控制**
（停止单项、改画质、断线重连），不回退到「每次订阅都弹确认」。

## 3. 文件职责重组

| 文件（新） | 职责 | 来源 |
|---|---|---|
| `state.ts` | 类型契约 + 单一状态源（全部运行时状态集中声明）+ 常量 | 原 state.ts 收敛，吸收散落变量（recvStreamId/pendingApprove/localStreams） |
| `ui.ts` | DOM 助手 / 图标 / canvas 绘制（不变） | 原 ui.ts |
| `discovery.ts` | 锚点（start_relay）+ mDNS 扫描 + 手动添加 + 设备图渲染 + 在线共享聚合 | 原 grid.ts 前半（含设备/流渲染） |
| `publish.ts` | 出站发布：广播弹窗 + 推流生命周期（startStreamWith/stopStream/pollStatus） | 原 send.ts + grid.ts 广播段 |
| `subscribe.ts` | 入站订阅：订阅流 + 接收统计 + 共享面板渲染 + 电脑端收麦克风（统一 beginAwaitMicStream） | 原 watch.ts + grid.ts 收麦克风段 |
| `negotiate.ts` | B2.5 凭证自动协商 + 定向推流 + 授权弹窗 | 原 grid.ts 协商段 |
| `firewall.ts` | 防火墙自检 / 一键放行 | 原 main.ts 防火墙段 |
| `main.ts` | 初始化 + 事件绑定（瘦身） | 原 main.ts |

加载顺序（`index.html`、`tsconfig.json`、`test-frontend.mjs` 三方同步更新）：
`state → ui → discovery → subscribe → publish → negotiate → firewall → main`

## 4. 语义收敛

- **订阅**：入站统一叫「订阅」（`subscribe.ts`），`startReceive` 是唯一订阅入口；
  `startMicReceive`（B2 电脑端签凭证）与协商允许 `respondApprove` 复用统一的
  `beginAwaitMicStream(streamId)`（设 micRecv + 轮询等流出现 + 自动订阅），
  消除两处重复的"设状态 + 轮询 + 自动接收"。
- **发布**：出站统一叫「发布」（`publish.ts`）。
- **状态命名语义化**：`streaming`→`publishing`、`starting`→`publishStarting`、
  `streamInfo`→`publishInfo`，明确"发布/启动"语义。
- **能力**（后续接缝）：后端内核已有 `CapabilityDescriptor` 能力模型，mDNS TXT
  能力清单打通（F1.2）后，前端设备卡片再展示"能力徽标"（当前展示"角色"）。

## 5. 交付标准

- `npx tsc` strict 通过；`app/*.js` 与 `*.ts` 同步（check-frontend.sh）。
- `node scripts/test-frontend.mjs` 41 项断言全绿（行为不回归）。
- `scripts/check.sh --quick` 全绿（rustfmt/clippy/tsc/js 同步）。

