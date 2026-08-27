//! uiautomator 视图树解析 + UI 状态输出（平台桥；XML 手写扫描，零依赖）。

use super::device::{adb_sh, pick_device, screenshot_to};

/// uiautomator dump XML 里的一个节点（文本 + 描述 + bounds）。
pub(crate) struct UiNode {
    pub(crate) text: String,
    pub(crate) desc: String,
    pub(crate) bounds: String,
}

/// 手机 UI 状态：截图 + 视图树文本（uiautomator dump）。
pub(crate) async fn ui_status(out: &str) -> anyhow::Result<()> {
    let serial = pick_device().await?;
    let n = screenshot_to(&serial, out).await?;
    println!("截图: {out}（{n} 字节）");
    match dump_ui_text(&serial).await {
        Ok(lines) if !lines.is_empty() => {
            println!("视图树文本（uiautomator dump）：");
            for l in lines {
                println!("  {l}");
            }
        }
        Ok(_) => println!("视图树无可见文本（WebView 页面内容通常不暴露给 uiautomator；截图见上）"),
        Err(e) => println!("uiautomator dump 失败: {e:#}（截图仍可看 UI）"),
    }
    Ok(())
}

/// `adb shell uiautomator dump` → 解析 XML 里的 text / content-desc 文本节点，
/// 按视图树顺序返回非空文本行。
async fn dump_ui_text(serial: &str) -> anyhow::Result<Vec<String>> {
    let path = "/sdcard/stross_ui.xml";
    let _ = adb_sh(serial, &format!("rm -f {path}")).await;
    let dump = adb_sh(serial, &format!("uiautomator dump {path}")).await?;
    if dump.contains("ERROR") || dump.contains("error") {
        anyhow::bail!("{dump}");
    }
    let xml = adb_sh(serial, &format!("cat {path}")).await?;
    let _ = adb_sh(serial, &format!("rm -f {path}")).await;
    // text 与 content-desc 都可能承载用户可见文本；空白值跳过
    let mut out = Vec::new();
    for s in collect_attr("text", &xml)
        .into_iter()
        .chain(collect_attr("content-desc", &xml))
    {
        let s = decode_xml(&s);
        if !s.trim().is_empty() {
            out.push(s);
        }
    }
    // 去重保序（WebView 常重复暴露同文本）
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.clone()));
    Ok(out)
}

/// 把 uiautomator dump XML 解析为节点列表（不引入 XML 依赖）。
pub(crate) fn ui_nodes(xml: &str) -> Vec<UiNode> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(i) = rest.find("<node") {
        let seg = &rest[i + 5..];
        let end = seg.find('>').unwrap_or(seg.len());
        let tag = &seg[..end];
        out.push(UiNode {
            text: attr_value(tag, "text"),
            desc: attr_value(tag, "content-desc"),
            bounds: attr_value(tag, "bounds"),
        });
        rest = &seg[end + 1..];
    }
    out
}

/// 取标签里 `name="..."` 的属性值（简单扫描，空值=缺）。
fn attr_value(tag: &str, name: &str) -> String {
    let needle = format!("{name}=\"");
    let Some(i) = tag.find(&needle) else {
        return String::new();
    };
    let tail = &tag[i + needle.len()..];
    let end = tail.find('"').unwrap_or(0);
    tail[..end].to_string()
}

/// 解析 `bounds="[x1,y1][x2,y2]"` → 中心坐标。
pub(crate) fn bounds_center(s: &str) -> Option<(u32, u32)> {
    let body = s.trim().strip_prefix('[')?.strip_suffix(']')?;
    let (a, b) = body.split_once("][")?;
    let (x1, y1) = a.split_once(',')?;
    let (x2, y2) = b.split_once(',')?;
    let (x1, y1, x2, y2): (u32, u32, u32, u32) = (
        x1.parse().ok()?,
        y1.parse().ok()?,
        x2.parse().ok()?,
        y2.parse().ok()?,
    );
    Some(((x1 + x2) / 2, (y1 + y2) / 2))
}

/// bounds "[x1,y1][x2,y2]" 面积；无/畸形返回 0（零面积节点不可点）。
pub(crate) fn bounds_area(s: &str) -> u64 {
    let body = match s.trim().strip_prefix('[').and_then(|b| b.strip_suffix(']')) {
        Some(b) => b,
        None => return 0,
    };
    let Some((a, b)) = body.split_once("][") else {
        return 0;
    };
    let (x1, y1) = match a.split_once(',') {
        Some(v) => v,
        None => return 0,
    };
    let (x2, y2) = match b.split_once(',') {
        Some(v) => v,
        None => return 0,
    };
    let (Ok(x1), Ok(y1), Ok(x2), Ok(y2)): (
        Result<u64, _>,
        Result<u64, _>,
        Result<u64, _>,
        Result<u64, _>,
    ) = (x1.parse(), y1.parse(), x2.parse(), y2.parse()) else {
        return 0;
    };
    let (w, h) = (x2.abs_diff(x1), y2.abs_diff(y1));
    w * h
}

/// 扫描 XML 里所有 `attr="..."` 的属性值（顺序保持；避免为一次解析引入
/// regex 依赖）。
fn collect_attr(attr: &str, text: &str) -> Vec<String> {
    let needle = format!("{attr}=\"");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(&needle) {
        let start = i + needle.len();
        let tail = &rest[start..];
        if let Some(end) = tail.find('"') {
            out.push(tail[..end].to_string());
            rest = &tail[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// 基础 XML 实体解码（&amp; &quot; &lt; &gt;）。
fn decode_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_attr_picks_text_in_order() {
        // 真实 uiautomator 形态：text="" 空值后面还有其它属性，解析须跳过并继续
        let xml = r#"<node text="设备列表" class="h"/><node text="手机A" content-desc="点我"/><node text="" class="y"/>"#;
        assert_eq!(
            collect_attr("text", xml),
            vec!["设备列表".to_string(), "手机A".to_string(), "".to_string()]
        );
        assert_eq!(collect_attr("content-desc", xml), vec!["点我".to_string()]);
        // 截断/畸形输入不崩溃（值未闭合 → 该属性丢弃，正常终止）
        assert_eq!(
            collect_attr("text", "<node text=\"abc"),
            Vec::<String>::new()
        );
        assert_eq!(
            collect_attr("text", "<node text=\"abc\""),
            vec!["abc".to_string()]
        );
    }

    #[test]
    fn decode_xml_entities() {
        assert_eq!(
            decode_xml("a&amp;b&quot;c&lt;d&gt;e&apos;f"),
            "a&b\"c<d>e'f"
        );
    }

    #[test]
    fn attr_value_reads_and_misses() {
        let tag = r#"<node text="扫描" bounds="[0,1][2,3]"/>"#;
        assert_eq!(attr_value(tag, "text"), "扫描");
        assert_eq!(attr_value(tag, "bounds"), "[0,1][2,3]");
        assert_eq!(attr_value(tag, "content-desc"), "");
    }

    #[test]
    fn bounds_center_computes_middle() {
        assert_eq!(bounds_center("[0,0][100,200]"), Some((50, 100)));
        assert_eq!(bounds_center("[10,20][30,40]"), Some((20, 30)));
        assert_eq!(bounds_center("(0,0)(1,1)"), None);
        assert_eq!(bounds_center(""), None);
    }

    #[test]
    fn ui_nodes_extract_text_and_bounds() {
        let xml = r#"<?xml?><hierarchy><node text="设备" bounds="[1,1][2,2]"/><node content-desc="点我" bounds="[3,3][4,4]"/></hierarchy>"#;
        let nodes = ui_nodes(xml);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].text, "设备");
        assert_eq!(nodes[0].bounds, "[1,1][2,2]");
        assert_eq!(nodes[1].desc, "点我");
    }
}
