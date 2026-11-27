//! 凭据读取：只读环境变量与本机 auth.json 的 apiKey 字段。

/// 凭据环境变量名；非空时优先使用。
const API_KEY_ENV: &str = "COMMAND_CODE_API_KEY";

/// 读取插件可用的 apiKey：环境变量优先，其次本机 auth.json。
pub fn api_key() -> Option<String> {
    api_key_from(
        |name| std::env::var(name).ok(),
        std::env::var("HOME").ok(),
        |path| std::fs::read_to_string(path).ok(),
    )
}

/// 注入环境变量、HOME 与文件读取，便于测试读取顺序。
fn api_key_from(
    getter: impl Fn(&str) -> Option<String>,
    home: Option<String>,
    reader: impl Fn(&std::path::Path) -> Option<String>,
) -> Option<String> {
    if let Some(key) = key_from_env(&getter) {
        return Some(key);
    }
    let path = std::path::Path::new(&home?)
        .join(".commandcode")
        .join("auth.json");
    let json = reader(&path)?;
    key_from_auth_json(&json)
}

/// 从注入的 getter 读取 apiKey；空白值视为缺失。
pub fn key_from_env(getter: impl Fn(&str) -> Option<String>) -> Option<String> {
    non_empty(getter(API_KEY_ENV))
}

/// 解析 auth.json 顶层 apiKey；不读取文件中的其他字段。
pub fn key_from_auth_json(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    non_empty(value.get("apiKey")?.as_str().map(str::to_string))
}

fn non_empty(value: Option<String>) -> Option<String> {
    let value = value?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    fn getter_with(value: Option<&str>) -> impl Fn(&str) -> Option<String> {
        let owned = value.map(str::to_string);
        move |_| owned.clone()
    }

    const FILE_KEY_JSON: &str = r#"{"apiKey":"user_from_file"}"#;

    #[test]
    fn key_from_env_returns_non_empty_env_value() {
        assert_eq!(
            key_from_env(getter_with(Some("user_from_env"))).as_deref(),
            Some("user_from_env")
        );
    }

    #[test]
    fn key_from_env_reads_only_the_documented_variable() {
        let names = RefCell::new(Vec::new());
        let key = key_from_env(|name| {
            names.borrow_mut().push(name.to_string());
            Some("user_from_env".to_string())
        });
        assert_eq!(key.as_deref(), Some("user_from_env"));
        assert_eq!(names.borrow().as_slice(), ["COMMAND_CODE_API_KEY"]);
    }

    #[test]
    fn key_from_env_rejects_empty_whitespace_and_missing() {
        assert_eq!(key_from_env(getter_with(Some(""))), None);
        assert_eq!(key_from_env(getter_with(Some("   \n\t"))), None);
        assert_eq!(key_from_env(getter_with(None)), None);
    }

    #[test]
    fn key_from_auth_json_reads_top_level_api_key_only() {
        let json = r#"{
            "apiKey": "user_abc",
            "codex": {"access": "secret-access", "refresh": "secret-refresh"},
            "nested": {"apiKey": "user_wrong"}
        }"#;
        assert_eq!(key_from_auth_json(json).as_deref(), Some("user_abc"));
    }

    #[test]
    fn key_from_auth_json_trims_surrounding_whitespace() {
        assert_eq!(
            key_from_auth_json(r#"{"apiKey":"  user_x  "}"#).as_deref(),
            Some("user_x")
        );
    }

    #[test]
    fn key_from_auth_json_rejects_missing_empty_nonstring_and_invalid() {
        assert_eq!(key_from_auth_json(r#"{"codex":{"access":"x"}}"#), None);
        assert_eq!(key_from_auth_json(r#"{"apiKey":""}"#), None);
        assert_eq!(key_from_auth_json(r#"{"apiKey":"   "}"#), None);
        assert_eq!(key_from_auth_json(r#"{"apiKey":123}"#), None);
        assert_eq!(key_from_auth_json(r#"{"apiKey":null}"#), None);
        assert_eq!(key_from_auth_json(r#"{"apiKey":{"value":"x"}}"#), None);
        assert_eq!(
            key_from_auth_json(r#"{"nested":{"apiKey":"user_x"}}"#),
            None
        );
        assert_eq!(key_from_auth_json("not json"), None);
        assert_eq!(key_from_auth_json("[]"), None);
        assert_eq!(key_from_auth_json(""), None);
    }

    #[test]
    fn api_key_from_prefers_env_and_does_not_read_file() {
        let reads = RefCell::new(0);
        let key = api_key_from(
            getter_with(Some("user_from_env")),
            Some("/home/test".to_string()),
            |_| {
                *reads.borrow_mut() += 1;
                Some(FILE_KEY_JSON.to_string())
            },
        );
        assert_eq!(key.as_deref(), Some("user_from_env"));
        assert_eq!(*reads.borrow(), 0);
    }

    #[test]
    fn api_key_from_falls_back_to_auth_json_when_env_empty() {
        let key = api_key_from(
            getter_with(Some("   ")),
            Some("/home/test".to_string()),
            |_| Some(FILE_KEY_JSON.to_string()),
        );
        assert_eq!(key.as_deref(), Some("user_from_file"));
    }

    #[test]
    fn api_key_from_reads_the_expected_auth_json_path() {
        let paths = RefCell::new(Vec::new());
        let key = api_key_from(getter_with(None), Some("/home/test".to_string()), |path| {
            paths.borrow_mut().push(path.to_path_buf());
            Some(FILE_KEY_JSON.to_string())
        });
        assert_eq!(key.as_deref(), Some("user_from_file"));
        assert_eq!(
            paths.borrow().as_slice(),
            [PathBuf::from("/home/test/.commandcode/auth.json")]
        );
    }

    #[test]
    fn api_key_from_returns_none_when_both_sources_missing() {
        assert_eq!(
            api_key_from(getter_with(None), Some("/home/test".to_string()), |_| None),
            None
        );
        // HOME 缺失时不再拼路径，也不读文件。
        assert_eq!(
            api_key_from(getter_with(None), None, |_| {
                panic!("HOME 缺失时不应读文件")
            }),
            None
        );
    }

    #[test]
    fn api_key_from_ignores_oauth_tokens_without_api_key() {
        let json = r#"{"codex":{"access":"access","refresh":"refresh"},"token":"oauth"}"#;
        assert_eq!(
            api_key_from(getter_with(None), Some("/home/test".to_string()), |_| {
                Some(json.to_string())
            }),
            None
        );
    }
}
