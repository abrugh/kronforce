use std::collections::HashMap;

use crate::db::Db;
use crate::db::models::{EventSeverity, ExecutionStatus, JobNotificationConfig};
use serde::{Deserialize, Serialize};

/// SMTP email delivery configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailConfig {
    pub enabled: bool,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub username: String,
    pub password: String,
    pub from: String,
    #[serde(default = "default_true")]
    pub tls: bool,
}

/// SMS delivery configuration via an HTTP webhook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmsConfig {
    pub enabled: bool,
    pub webhook_url: String,
    pub auth_user: Option<String>,
    pub auth_pass: Option<String>,
    pub from_number: Option<String>,
}

/// Webhook notification configuration (Slack, Teams, PagerDuty, generic, custom template).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub enabled: bool,
    pub url: String,
    /// Webhook format: "slack", "teams", "pagerduty", "discord", "custom", "passthrough", or "generic" (default).
    #[serde(default = "default_generic")]
    pub format: String,
    /// Optional custom headers (e.g., Authorization).
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Custom JSON template with `{{placeholder}}` variables. Used when format is "custom".
    /// Available placeholders: {{subject}}, {{body}}, {{job_name}}, {{status}},
    /// {{execution_id}}, {{stdout}}, {{stderr}}, {{timestamp}}.
    #[serde(default)]
    pub template: Option<String>,
}

/// Context passed to webhook template rendering. Contains all available placeholder values.
#[derive(Debug, Clone, Default)]
pub struct WebhookContext {
    pub subject: String,
    pub body: String,
    pub job_name: Option<String>,
    pub status: Option<String>,
    pub execution_id: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub timestamp: String,
}

fn default_generic() -> String {
    "generic".to_string()
}

/// Email addresses and phone numbers that receive notifications.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotificationRecipients {
    #[serde(default)]
    pub emails: Vec<String>,
    #[serde(default)]
    pub phones: Vec<String>,
}

/// Toggles for system-level alert notifications (e.g., agent going offline).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemAlerts {
    #[serde(default)]
    pub agent_offline: bool,
}

fn default_true() -> bool {
    true
}

/// Loads the email configuration from the database, returning `None` if disabled.
pub fn load_email_config(db: &Db) -> Option<EmailConfig> {
    db.get_setting("notification_email")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|c: &EmailConfig| c.enabled)
}

/// Loads the SMS configuration from the database, returning `None` if disabled.
pub fn load_sms_config(db: &Db) -> Option<SmsConfig> {
    db.get_setting("notification_sms")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|c: &SmsConfig| c.enabled)
}

/// Loads the webhook configuration from the database, returning `None` if disabled.
pub fn load_webhook_config(db: &Db) -> Option<WebhookConfig> {
    db.get_setting("notification_webhook")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(|c: &WebhookConfig| c.enabled && !c.url.is_empty())
}

/// Loads the global notification recipients from the database.
pub fn load_recipients(db: &Db) -> NotificationRecipients {
    db.get_setting("notification_recipients")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Loads the system alert toggle settings from the database.
pub fn load_system_alerts(db: &Db) -> SystemAlerts {
    db.get_setting("notification_system_alerts")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Sends a notification via all enabled channels (email, SMS, webhook) to the given or global recipients.
pub async fn send_notification(
    db: &Db,
    subject: &str,
    body: &str,
    recipient_override: Option<&NotificationRecipients>,
    webhook_context: Option<WebhookContext>,
) {
    let recipients = match recipient_override {
        Some(r) if !r.emails.is_empty() || !r.phones.is_empty() => r.clone(),
        _ => load_recipients(db),
    };

    if let Some(email_config) = load_email_config(db)
        && !recipients.emails.is_empty()
    {
        let to = recipients.emails.clone();
        let subj = subject.to_string();
        let bod = body.to_string();
        let db_clone = db.clone();
        tokio::spawn(async move {
            match send_email(&email_config, &to, &subj, &bod).await {
                Ok(_) => {
                    let _ = db_clone.log_event(
                        "notification.sent",
                        EventSeverity::Info,
                        &format!("Email sent to {} recipient(s): {}", to.len(), subj),
                        None,
                        None,
                    );
                }
                Err(e) => {
                    let _ = db_clone.log_event(
                        "notification.failed",
                        EventSeverity::Error,
                        &format!("Email failed: {} — {}", subj, e),
                        None,
                        None,
                    );
                }
            }
        });
    }

    if let Some(sms_config) = load_sms_config(db)
        && !recipients.phones.is_empty()
    {
        let to = recipients.phones.clone();
        let bod = body.to_string();
        let subj = subject.to_string();
        let db_clone = db.clone();
        tokio::spawn(async move {
            match send_sms(&sms_config, &to, &bod).await {
                Ok(_) => {
                    let _ = db_clone.log_event(
                        "notification.sent",
                        EventSeverity::Info,
                        &format!("SMS sent to {} recipient(s): {}", to.len(), subj),
                        None,
                        None,
                    );
                }
                Err(e) => {
                    let _ = db_clone.log_event(
                        "notification.failed",
                        EventSeverity::Error,
                        &format!("SMS failed: {} — {}", subj, e),
                        None,
                        None,
                    );
                }
            }
        });
    }

    if let Some(webhook_config) = load_webhook_config(db) {
        let subj = subject.to_string();
        let bod = body.to_string();
        let db_clone = db.clone();
        let ctx = webhook_context.unwrap_or_else(|| WebhookContext {
            subject: subj.clone(),
            body: bod.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        });
        tokio::spawn(async move {
            match send_webhook(&webhook_config, &subj, &bod, Some(&ctx)).await {
                Ok(_) => {
                    let _ = db_clone.log_event(
                        "notification.sent",
                        EventSeverity::Info,
                        &format!("Webhook sent ({}): {}", webhook_config.format, subj),
                        None,
                        None,
                    );
                }
                Err(e) => {
                    let _ = db_clone.log_event(
                        "notification.failed",
                        EventSeverity::Error,
                        &format!("Webhook failed: {} — {}", subj, e),
                        None,
                        None,
                    );
                }
            }
        });
    }
}

/// Check if notification should be sent for a completed execution, and send it if so.
#[allow(clippy::too_many_arguments)]
pub async fn notify_execution_complete(
    db: &Db,
    notif: &JobNotificationConfig,
    job_name: &str,
    exec_id_short: &str,
    exec_status: ExecutionStatus,
    stderr_excerpt: &str,
    stdout: &str,
    stderr: &str,
) {
    let should_notify = match exec_status {
        ExecutionStatus::Failed | ExecutionStatus::TimedOut => {
            notif.on_failure || notif.on_assertion_failure
        }
        ExecutionStatus::Succeeded => notif.on_success,
        _ => false,
    };
    if !should_notify {
        return;
    }
    let subject = format!(
        "[Kronforce] Job '{}' {}",
        job_name,
        match exec_status {
            ExecutionStatus::Succeeded => "succeeded",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::TimedOut => "timed out",
            _ => "completed",
        }
    );

    // Determine whether to include full output
    let include_output = match notif.email_output.as_deref() {
        Some("always") => true,
        Some("failure") => matches!(
            exec_status,
            ExecutionStatus::Failed | ExecutionStatus::TimedOut
        ),
        _ => false,
    };

    let mut body = format!(
        "Job: {}\nStatus: {:?}\nExecution: {}\nTime: {}\n",
        job_name,
        exec_status,
        exec_id_short,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
    );
    if include_output {
        if !stdout.is_empty() {
            body.push_str(&format!(
                "\n--- Output ---\n{}\n",
                &stdout[..stdout.len().min(50_000)]
            ));
        }
        if !stderr.is_empty() {
            body.push_str(&format!(
                "\n--- Error ---\n{}\n",
                &stderr[..stderr.len().min(50_000)]
            ));
        }
    } else if !stderr_excerpt.is_empty() {
        body.push_str(&format!("\nError output:\n{}", stderr_excerpt));
    }

    let recipients = notif.recipients.as_ref().map(|r| NotificationRecipients {
        emails: r.emails.clone(),
        phones: r.phones.clone(),
    });
    let context = WebhookContext {
        subject: subject.clone(),
        body: body.clone(),
        job_name: Some(job_name.to_string()),
        status: Some(match exec_status {
            ExecutionStatus::Succeeded => "succeeded".to_string(),
            ExecutionStatus::Failed => "failed".to_string(),
            ExecutionStatus::TimedOut => "timed_out".to_string(),
            _ => "completed".to_string(),
        }),
        execution_id: Some(exec_id_short.to_string()),
        stdout: Some(stdout.to_string()),
        stderr: Some(stderr.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
    };
    send_notification(db, &subject, &body, recipients.as_ref(), Some(context)).await;
}

/// Sends an email to one or more recipients via SMTP.
pub async fn send_email(
    config: &EmailConfig,
    to: &[String],
    subject: &str,
    body: &str,
) -> Result<(), String> {
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{Message, SmtpTransport, Transport};

    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let mailer = if config.tls {
        SmtpTransport::starttls_relay(&config.smtp_host)
            .map_err(|e| format!("SMTP relay error: {e}"))?
            .port(config.smtp_port)
            .credentials(creds)
            .build()
    } else {
        SmtpTransport::builder_dangerous(&config.smtp_host)
            .port(config.smtp_port)
            .credentials(creds)
            .build()
    };

    for recipient in to {
        let email = Message::builder()
            .from(
                config
                    .from
                    .parse()
                    .map_err(|e| format!("bad from address: {e}"))?,
            )
            .to(recipient
                .parse()
                .map_err(|e| format!("bad to address '{}': {e}", recipient))?)
            .subject(subject)
            .body(body.to_string())
            .map_err(|e| format!("email build error: {e}"))?;

        mailer
            .send(&email)
            .map_err(|e| format!("SMTP send error: {e}"))?;
    }

    Ok(())
}

/// Sends an SMS to one or more phone numbers via the configured webhook.
pub async fn send_sms(config: &SmsConfig, to: &[String], body: &str) -> Result<(), String> {
    let client = reqwest::Client::new();

    for phone in to {
        let mut req = client.post(&config.webhook_url).json(&serde_json::json!({
            "To": phone,
            "From": config.from_number.as_deref().unwrap_or(""),
            "Body": body,
        }));

        if let (Some(user), Some(pass)) = (&config.auth_user, &config.auth_pass) {
            req = req.basic_auth(user, Some(pass));
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("SMS webhook error: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("SMS webhook returned {}: {}", status, text));
        }
    }

    Ok(())
}

/// Renders a template string by replacing `{{placeholder}}` markers with values from the context.
fn render_template(template: &str, ctx: &WebhookContext) -> String {
    let mut result = template.to_string();
    result = result.replace("{{subject}}", &ctx.subject);
    result = result.replace("{{body}}", &ctx.body);
    result = result.replace("{{job_name}}", ctx.job_name.as_deref().unwrap_or(""));
    result = result.replace("{{status}}", ctx.status.as_deref().unwrap_or(""));
    result = result.replace(
        "{{execution_id}}",
        ctx.execution_id.as_deref().unwrap_or(""),
    );
    result = result.replace("{{stdout}}", ctx.stdout.as_deref().unwrap_or(""));
    result = result.replace("{{stderr}}", ctx.stderr.as_deref().unwrap_or(""));
    result = result.replace("{{timestamp}}", &ctx.timestamp);
    result
}

/// Sends a notification via a webhook (Slack, Teams, PagerDuty, Discord, custom template, or generic JSON POST).
pub async fn send_webhook(
    config: &WebhookConfig,
    subject: &str,
    body: &str,
    context: Option<&WebhookContext>,
) -> Result<(), String> {
    let client = reqwest::Client::new();

    // Build a default context if none provided
    let default_ctx = WebhookContext {
        subject: subject.to_string(),
        body: body.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    };
    let ctx = context.unwrap_or(&default_ctx);

    let payload = match config.format.as_str() {
        "slack" => serde_json::json!({
            "text": format!("*{}*\n{}", subject, body),
        }),
        "teams" => serde_json::json!({
            "title": subject,
            "text": body,
        }),
        "pagerduty" => serde_json::json!({
            "routing_key": config.headers.get("routing_key").cloned().unwrap_or_default(),
            "event_action": "trigger",
            "payload": {
                "summary": subject,
                "source": "kronforce",
                "severity": "error",
                "custom_details": {
                    "body": body,
                }
            }
        }),
        "discord" => {
            let color: u32 = if subject.contains("succeeded") {
                0x2ECC71 // green
            } else if subject.contains("timed out") {
                0xF39C12 // orange
            } else {
                0xE74C3C // red
            };
            serde_json::json!({
                "embeds": [{
                    "title": subject,
                    "description": body,
                    "color": color,
                    "footer": { "text": "Kronforce" },
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                }]
            })
        }
        "custom" => {
            let template = config
                .template
                .as_deref()
                .unwrap_or(r#"{"text": "{{subject}}\n{{body}}"}"#);
            let rendered = render_template(template, ctx);
            serde_json::from_str(&rendered).map_err(|e| {
                format!("custom template produced invalid JSON: {e}\nRendered: {rendered}")
            })?
        }
        "passthrough" => {
            // In passthrough mode, stdout IS the payload. If stdout is valid JSON, send it raw.
            // Falls back to generic format if stdout is empty or not valid JSON.
            let raw = ctx.stdout.as_deref().unwrap_or("").trim();
            if raw.is_empty() {
                serde_json::json!({
                    "subject": subject,
                    "body": body,
                    "source": "kronforce",
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                })
            } else {
                serde_json::from_str(raw).map_err(|e| {
                    format!("passthrough mode requires stdout to be valid JSON: {e}")
                })?
            }
        }
        _ => serde_json::json!({
            "subject": subject,
            "body": body,
            "source": "kronforce",
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }),
    };

    let mut req = client.post(&config.url).json(&payload);
    for (key, value) in &config.headers {
        if key != "routing_key" {
            req = req.header(key.as_str(), value.as_str());
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("webhook error: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("webhook returned {}: {}", status, text));
    }

    Ok(())
}

/// Send a test notification to verify channel configuration
pub async fn send_test(db: &Db) -> Result<String, String> {
    let recipients = load_recipients(db);
    let mut results = Vec::new();

    if let Some(email_config) = load_email_config(db) {
        if let Some(first) = recipients.emails.first() {
            match send_email(
                &email_config,
                std::slice::from_ref(first),
                "[Kronforce] Test Notification",
                "This is a test notification from Kronforce.",
            )
            .await
            {
                Ok(_) => results.push(format!("Email sent to {}", first)),
                Err(e) => results.push(format!("Email failed: {}", e)),
            }
        } else {
            results.push("Email enabled but no recipients configured".to_string());
        }
    } else {
        results.push("Email channel not enabled".to_string());
    }

    if let Some(sms_config) = load_sms_config(db) {
        if let Some(first) = recipients.phones.first() {
            match send_sms(
                &sms_config,
                std::slice::from_ref(first),
                "[Kronforce] Test notification",
            )
            .await
            {
                Ok(_) => results.push(format!("SMS sent to {}", first)),
                Err(e) => results.push(format!("SMS failed: {}", e)),
            }
        } else {
            results.push("SMS enabled but no recipients configured".to_string());
        }
    } else {
        results.push("SMS channel not enabled".to_string());
    }

    if let Some(webhook_config) = load_webhook_config(db) {
        match send_webhook(
            &webhook_config,
            "[Kronforce] Test Notification",
            "This is a test notification from Kronforce.",
            None,
        )
        .await
        {
            Ok(_) => results.push(format!("Webhook sent ({})", webhook_config.format)),
            Err(e) => results.push(format!("Webhook failed: {}", e)),
        }
    } else {
        results.push("Webhook channel not enabled".to_string());
    }

    Ok(results.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_template_all_placeholders() {
        let ctx = WebhookContext {
            subject: "Job 'deploy' succeeded".to_string(),
            body: "All good".to_string(),
            job_name: Some("deploy".to_string()),
            status: Some("succeeded".to_string()),
            execution_id: Some("abc123".to_string()),
            stdout: Some("deployed v2.0".to_string()),
            stderr: Some("".to_string()),
            timestamp: "2026-06-08T12:00:00Z".to_string(),
        };
        let template = r#"{"title":"{{subject}}","output":"{{stdout}}","job":"{{job_name}}","status":"{{status}}","exec":"{{execution_id}}","ts":"{{timestamp}}"}"#;
        let rendered = render_template(template, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["title"], "Job 'deploy' succeeded");
        assert_eq!(parsed["output"], "deployed v2.0");
        assert_eq!(parsed["job"], "deploy");
        assert_eq!(parsed["status"], "succeeded");
        assert_eq!(parsed["exec"], "abc123");
        assert_eq!(parsed["ts"], "2026-06-08T12:00:00Z");
    }

    #[test]
    fn test_render_template_missing_optional_fields() {
        let ctx = WebhookContext {
            subject: "test".to_string(),
            body: "hello".to_string(),
            job_name: None,
            status: None,
            execution_id: None,
            stdout: None,
            stderr: None,
            timestamp: "now".to_string(),
        };
        let template = r#"{"msg":"{{subject}} {{job_name}}"}"#;
        let rendered = render_template(template, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["msg"], "test ");
    }

    #[test]
    fn test_render_template_discord_preset() {
        let ctx = WebhookContext {
            subject: "Build done".to_string(),
            body: "Success".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            ..Default::default()
        };
        let template = r#"{"embeds":[{"title":"{{subject}}","description":"{{body}}","timestamp":"{{timestamp}}"}]}"#;
        let rendered = render_template(template, &ctx);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["embeds"][0]["title"], "Build done");
        assert_eq!(parsed["embeds"][0]["description"], "Success");
    }
}
