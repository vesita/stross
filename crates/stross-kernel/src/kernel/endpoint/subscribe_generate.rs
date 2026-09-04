//! 订阅端点生成（v3 §2.2 策略注册表模式）：`EndpointClass` → 工厂 → 订阅端点。
//!
//! 模块拆分（v3 §7）：从统一注册表拆出的订阅端点生成域——工厂注册表
//! （[`SubscribeEndpointFactory`] / `default_factories`）与
//! `UnifiedRegistry::generate_subscribe_endpoint`。

use std::collections::HashMap;
use std::path::Path;

use stross_endpoint::contract::{EndpointClass, SubscribeEndpoint};
use stross_endpoint::subscribe::file::FileReceiveEndpoint;
use stross_endpoint::subscribe::media::MediaReceiveEndpoint;
use stross_proto::message::{EndpointId, SubscribeSpec};

use super::UnifiedRegistry;

/// 订阅端点生成工厂（v3 §2.2 策略注册表模式）：`EndpointClass` →
/// `(SubscribeSpec, out_dir)` → 订阅端点（未定义宿主返回 `None`）。
///
/// 键是**强类型 [`EndpointClass`] 枚举**（不是 String）；值是 trait 对象工厂，
/// 表由注册表持有、实现方注册——**新增端点类（Clipboard/Input/Service）只需
/// 注册工厂即可扩展，不碰生成分派逻辑**。
pub type SubscribeEndpointFactory =
    Box<dyn Fn(&SubscribeSpec, Option<&Path>) -> Option<Box<dyn SubscribeEndpoint>> + Send + Sync>;

/// 默认订阅端点生成工厂表（§2.2 策略注册表模式；构造处注册保持现状行为）：
/// * File → 文件订阅端（接收落盘到 `out_dir`）；
/// * Graph / Audio → 媒体订阅端（[`MediaReceiveEndpoint`]：收流 + 解码，
///   播放器入端点；`out_dir` 不适用）；
/// * 其余族（Clipboard/Input/Service）未注册 → `None`（后续按族补实现）。
pub(super) fn default_factories() -> HashMap<EndpointClass, SubscribeEndpointFactory> {
    let mut subscribe_factories: HashMap<EndpointClass, SubscribeEndpointFactory> = HashMap::new();
    subscribe_factories.insert(
        EndpointClass::File,
        Box::new(|spec, out_dir| {
            Some(Box::new(FileReceiveEndpoint::new(
                EndpointId::new(spec.kind, spec.endpoint_id),
                // 订阅端点 name 仅日志/调试展示（分享端展示名是注册表元数据，
                // 不进 SubscribeSpec wire；订阅端点生成后立即消费，不落展示）
                spec.kind.as_str().to_string(),
                out_dir
                    .map(Path::to_path_buf)
                    .unwrap_or_else(std::env::temp_dir),
            )))
        }),
    );
    for class in [EndpointClass::Graph, EndpointClass::Audio] {
        subscribe_factories.insert(
            class,
            Box::new(|spec, _out_dir| {
                Some(Box::new(MediaReceiveEndpoint::new(
                    EndpointId::new(spec.kind, spec.endpoint_id),
                    spec.kind.as_str().to_string(),
                    spec.kind,
                )))
            }),
        );
    }
    subscribe_factories
}

impl UnifiedRegistry {
    /// 订阅端点生成（docs/framework-v3.md §3「订阅端点生成」）：
    /// 按订阅目标端点的**能力族**（[`EndpointClass`]）查**工厂注册表**
    /// （v3 §2.2 策略注册表模式，[`SubscribeEndpointFactory`]）构造**统一的
    /// 族订阅端点**——内核不做类型分派（端点实现自驱动，与分享端 `share`
    /// 同构），也不再在 match 里硬编码具体类型。
    ///
    /// * 默认注册：`File` → 文件订阅端（[`FileReceiveEndpoint`]，接收落盘到
    ///   `out_dir`）、`Graph` / `Audio` → 媒体订阅端（[`MediaReceiveEndpoint`]，
    ///   收流 + 解码，播放器入端点；`out_dir` 不适用）；
    /// * 其余族（剪贴板/输入/服务）默认未注册 → `None`；**新增端点类只需
    ///   注册工厂即可扩展**（[`UnifiedRegistry::register_subscribe_factory`] /
    ///   [`Kernel::register_subscribe_factory`]），不碰本方法。
    pub fn generate_subscribe_endpoint(
        &self,
        spec: &SubscribeSpec,
        out_dir: Option<&Path>,
    ) -> Option<Box<dyn SubscribeEndpoint>> {
        let class = EndpointClass::from_kind(spec.kind);
        self.subscribe_factories
            .get(&class)
            .and_then(|f| f(spec, out_dir))
    }

    /// 注册订阅端点生成工厂（`EndpointClass` 键，强类型枚举；覆盖默认工厂）。
    /// 新增端点类（Clipboard/Input/Service）注册工厂即扩展（§2.2）。
    pub fn register_subscribe_factory(
        &mut self,
        class: EndpointClass,
        factory: SubscribeEndpointFactory,
    ) {
        self.subscribe_factories.insert(class, factory);
    }
}
