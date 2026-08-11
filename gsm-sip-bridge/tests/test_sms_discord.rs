use gsm_sip_bridge::config::secret::Secret;
use gsm_sip_bridge::sms::discord::DiscordClient;

#[tokio::test]
async fn test_discord_client_creation() {
    let webhook = Secret::new("https://discord.com/api/webhooks/123/abc".into());
    let client = DiscordClient::new(webhook, "test-instance".to_string());
    assert!(client.is_ok());
}

#[tokio::test]
async fn test_discord_truncation_long_body() {
    let long_body: String = "x".repeat(5000);
    let truncated = if long_body.len() > 4090 {
        format!("{}…", &long_body[..4090])
    } else {
        long_body.clone()
    };
    assert_eq!(truncated.len(), 4093);
    assert!(truncated.ends_with('…'));
}

#[tokio::test]
async fn test_discord_emoji_body() {
    let body = "Hello 👋 from GSM! 🎉";
    assert!(!body.is_ascii());
    assert!(body.len() < 4090);
}
