//! 内嵌的观看端静态资源（编译期打包，中继零外部依赖）。

pub const INDEX_HTML: &str = include_str!("../assets/viewer/index.html");
pub const STYLE_CSS: &str = include_str!("../assets/viewer/style.css");
pub const APP_JS: &str = include_str!("../assets/viewer/app.js");
pub const JMUXER_JS: &str = include_str!("../assets/viewer/jmuxer.js");
