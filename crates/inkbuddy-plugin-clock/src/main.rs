//! 当前日期与时间。
//!
//! 样式由宿主在渲染参数里给出（视图区域选的子样式）；没给时用
//! `Style::all()` 的第一项，也就是经典排版。
//!
//! dot 和 calendar 输出带 class 的 HTML 块，样式表在宿主页面里。
//! 插件只负责给元素命名和填时间，不决定尺寸与颜色。

use anyhow::Result;
use chrono::{Datelike, Local};
use inkbuddy_protocol::{
    Block, Emphasis, KeyValue, Line, Method, PluginInfo, RenderParams, RenderResult, Request,
    Response, VariantInfo, PROTOCOL_VERSION,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PLUGIN_ID: &str = "clock";
const REFRESH_MS: u64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Classic,
    Dot,
    Calendar,
}

impl Style {
    fn from_id(id: &str) -> Option<Style> {
        match id.trim().to_ascii_lowercase().as_str() {
            "classic" => Some(Style::Classic),
            "dot" => Some(Style::Dot),
            "calendar" => Some(Style::Calendar),
            _ => None,
        }
    }

    fn all() -> Vec<VariantInfo> {
        [
            ("classic", "经典"),
            ("dot", "点阵"),
            ("calendar", "台历"),
        ]
        .into_iter()
        .map(|(id, name)| VariantInfo {
            id: id.to_string(),
            name: name.to_string(),
        })
        .collect()
    }
}

/// 本帧的样式：视图区域带合法 variant 时用它，否则用 `Style::all()` 的
/// 第一项。未知的 id 同样回退第一项，避免旧视图里的样式名让时钟消失。
fn render_style(params: &RenderParams) -> Style {
    params
        .variant
        .as_deref()
        .and_then(Style::from_id)
        .unwrap_or(Style::Classic)
}

/// 一帧里要填的全部时间字段。三种样式共用，避免各自格式化一遍。
struct Clock {
    hm: String,
    ymd: String,
    date_cn: String,
    week_cn: String,
    month_cn: String,
    day: String,
}

impl Clock {
    fn now() -> Self {
        let now = Local::now();
        let week_cn = match now.weekday() {
            chrono::Weekday::Mon => "星期一",
            chrono::Weekday::Tue => "星期二",
            chrono::Weekday::Wed => "星期三",
            chrono::Weekday::Thu => "星期四",
            chrono::Weekday::Fri => "星期五",
            chrono::Weekday::Sat => "星期六",
            chrono::Weekday::Sun => "星期日",
        };
        let month = now.month();
        let day = now.day();
        Self {
            hm: now.format("%H:%M").to_string(),
            ymd: now.format("%Y-%m-%d").to_string(),
            date_cn: format!("{month}月{day}日"),
            week_cn: week_cn.to_string(),
            month_cn: format!("{month}月"),
            day: format!("{day:02}"),
        }
    }
}

fn html_block(html: String) -> RenderResult {
    RenderResult {
        blocks: vec![Block::Html { html }],
        refresh_ms: Some(REFRESH_MS),
    }
}

/// 原来的排版：一行主文本 + 日期、星期键值对。
fn classic(clock: &Clock) -> RenderResult {
    RenderResult {
        blocks: vec![
            Block::Lines {
                lines: vec![Line::new(clock.hm.clone(), Emphasis::Primary)],
            },
            Block::KeyValue {
                items: vec![
                    KeyValue {
                        key: "日期".to_string(),
                        value: clock.ymd.clone(),
                    },
                    KeyValue {
                        key: "星期".to_string(),
                        value: clock.week_cn.clone(),
                    },
                ],
            },
        ],
        refresh_ms: Some(REFRESH_MS),
    }
}

/// 点阵：只有时间、日期和星期，都是要读的信息。
/// data-ink-height 按 ink.html 的点阵尺寸累加：时间 16vw×1.05 + 脚注 3.4vw
/// + 上下 padding 8vw + 行间距 3vw = 31.2。
fn dot(clock: &Clock) -> RenderResult {
    let html = format!(
        "<div class=\"block clock-dot\" data-ink-height=\"31.2\">\
           <div class=\"clock-dot-time\" data-ink-role=\"primary\">{hm}</div>\
           <div class=\"clock-dot-foot\" data-ink-role=\"muted\"><span>{date}</span>\
             <span class=\"clock-dot-sep\">·</span><span>{week}</span></div>\
         </div>",
        hm = clock.hm,
        date = clock.date_cn,
        week = clock.week_cn,
    );
    html_block(html)
}

/// 台历：月份与年份并排在一行，中间一个大「日」，底部一行是星期和时间。
/// 所有元素都在正常文档流里，靠容器本身的 flex 排布，不用视口绝对定位；
/// 这样放到多小的区域里都不会被裁掉。
fn calendar(clock: &Clock) -> RenderResult {
    let year = clock.ymd.get(..4).unwrap_or("");
    // data-ink-height 按 ink.html 的台历尺寸累加：印章 8vw + 日期 26vw×0.9
    // + 脚行 5vw + 上下 padding 6vw + 两道行间距 7vw = 49.4。
    let html = format!(
        "<div class=\"block clock-cal\" data-ink-height=\"49.4\">\
           <div class=\"clock-cal-head\" data-ink-role=\"muted\">\
             <span class=\"clock-cal-ym\">{year} / {month}</span>\
             <span class=\"clock-cal-seal\">今日</span></div>\
           <div class=\"clock-cal-day\" data-ink-role=\"primary\">{day}</div>\
           <div class=\"clock-cal-foot\" data-ink-role=\"normal\">\
             <span class=\"clock-cal-week\">{week}</span>\
             <span class=\"clock-cal-hm\">{hm}</span></div>\
         </div>",
        year = year,
        month = clock.month_cn,
        day = clock.day,
        week = clock.week_cn,
        hm = clock.hm,
    );
    html_block(html)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let req: Request = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let resp = match req.method {
            Method::Ping => Response::empty_ok(req.id),
            Method::Describe => Response::success(
                req.id,
                &PluginInfo {
                    id: PLUGIN_ID.to_string(),
                    name: "时钟".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    description: "当前日期与时间，支持经典、点阵与台历样式".to_string(),
                    protocol: PROTOCOL_VERSION,
                    variants: Style::all(),
                },
            ),
            Method::Render => {
                let params: RenderParams = serde_json::from_value(req.params.clone())
                    .unwrap_or_default();
                let clock = Clock::now();
                let result = match render_style(&params) {
                    Style::Classic => classic(&clock),
                    Style::Dot => dot(&clock),
                    Style::Calendar => calendar(&clock),
                };
                Response::success(req.id, &result)
            }
            Method::Configure => Response::empty_ok(req.id),
            Method::Shutdown => {
                let out = serde_json::to_string(&Response::empty_ok(req.id))?;
                stdout.write_all(format!("{out}\n").as_bytes()).await?;
                stdout.flush().await?;
                break;
            }
        };

        let out = serde_json::to_string(&resp)?;
        stdout.write_all(format!("{out}\n").as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Clock {
        Clock {
            hm: "21:22".into(),
            ymd: "2026-11-26".into(),
            date_cn: "11月26日".into(),
            week_cn: "星期四".into(),
            month_cn: "11月".into(),
            day: "26".into(),
        }
    }

    #[test]
    fn classic_keeps_the_original_blocks() {
        let r = classic(&sample());
        assert_eq!(r.blocks.len(), 2);
        assert!(matches!(r.blocks[0], Block::Lines { .. }));
        assert!(matches!(r.blocks[1], Block::KeyValue { .. }));
        assert_eq!(r.refresh_ms, Some(REFRESH_MS));
    }

    #[test]
    fn dot_and_calendar_emit_classed_markup() {
        let mut count = 0;
        for result in [dot(&sample()), calendar(&sample())] {
            for block in &result.blocks {
                let Block::Html { html } = block else {
                    panic!("dot and calendar must emit HTML blocks");
                };
                assert!(!html.to_ascii_lowercase().contains("<script"));
                assert!(html.contains("class=\"clock-"));
                assert!(html.contains("21:22"));
                count += 1;
            }
        }
        assert_eq!(count, 2);
    }

    #[test]
    fn dot_has_no_decorative_labels() {
        let Block::Html { html } = &dot(&sample()).blocks[0] else {
            panic!("dot must be an HTML block");
        };
        let upper = html.to_ascii_uppercase();
        assert!(!upper.contains("DOT MATRIX"), "dot label must not contain decorative text");
        assert!(!upper.contains("5×7"), "dot label must not contain pixel-size notation");
        // 只保留要读的信息：时间、日期、星期
        assert!(html.contains("21:22"));
        assert!(html.contains("11月26日"));
        assert!(html.contains("星期四"));
    }

    #[test]
    fn html_styles_declare_height_and_roles() {
        for (result, height) in [(dot(&sample()), "31.2"), (calendar(&sample()), "49.4")] {
            let Block::Html { html } = &result.blocks[0] else {
                panic!("must be an HTML block");
            };
            assert!(html.contains(&format!("data-ink-height=\"{height}\"")));
            assert!(html.contains("data-ink-role=\"primary\""));
        }
    }

    #[test]
    fn calendar_shows_year_and_weekday() {
        let r = calendar(&sample());
        let Block::Html { html } = &r.blocks[0] else {
            panic!("calendar must be an HTML block");
        };
        assert!(html.contains("2026"));
        assert!(html.contains("11月"));
        assert!(html.contains("今日"));
        assert!(html.contains("星期四"));
        // 台历不能依赖视口绝对定位，所有元素都在正常流里
        assert!(!html.contains("position"));
    }

    #[test]
    fn region_variant_falls_back_to_first_style() {
        let from_region = |v: Option<&str>| RenderParams {
            variant: v.map(str::to_string),
            ..RenderParams::default()
        };
        assert_eq!(render_style(&from_region(Some("classic"))), Style::Classic);
        assert_eq!(render_style(&from_region(Some("DOT"))), Style::Dot);
        assert_eq!(render_style(&from_region(Some("calendar"))), Style::Calendar);
        // 未指定和未知 id 都用声明列表里的第一项
        assert_eq!(render_style(&from_region(None)), Style::Classic);
        assert_eq!(render_style(&from_region(Some("future-style"))), Style::Classic);
        assert_eq!(Style::from_id(Style::all()[0].id.as_str()), Some(Style::Classic));
    }

    #[test]
    fn describe_lists_three_styles() {
        let variants = Style::all();
        assert_eq!(variants.len(), 3);
        assert_eq!(variants[0].id, "classic");
        assert_eq!(variants[1].name, "点阵");
        assert_eq!(variants[2].id, "calendar");
    }
}
