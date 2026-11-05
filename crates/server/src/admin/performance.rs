//! On-demand server CPU (including workers), main-process memory and live playback.
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use super::presentation::{
    apply_template, html_error, load_template, render_admin_response, PageLayout,
};
use super::{admin_user_from_headers, is_admin};
use crate::http::{json_error_response, redirect_to};
use crate::process_metrics::{CpuStatus, Snapshot};
use crate::state::AppState;

fn authorize(state: &AppState, headers: &HeaderMap, json: bool) -> Result<(), Response> {
    let error = |status, message: String| {
        if json {
            json_error_response(status, message)
        } else {
            html_error(state, status, message)
        }
    };
    let has_admin = state.auth.has_admin().map_err(|err| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auth error: {err}"),
        )
    })?;
    if !has_admin {
        return Err(if json {
            error(StatusCode::UNAUTHORIZED, "admin setup required".into())
        } else {
            redirect_to("/setup")
        });
    }
    let user = admin_user_from_headers(state, headers).map_err(|err| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("auth error: {err}"),
        )
    })?;
    match user {
        Some(user) if is_admin(&user) => Ok(()),
        Some(_) => Err(error(
            StatusCode::FORBIDDEN,
            "administrator required".into(),
        )),
        None if json => Err(error(
            StatusCode::UNAUTHORIZED,
            "sign in as an administrator".into(),
        )),
        None => Err(redirect_to("/login")),
    }
}

async fn snapshot(state: &AppState) -> Result<Snapshot, String> {
    let metrics = state.process_metrics.clone();
    // Native process queries and /proc reads stay off the async executor.
    state
        .shutdown
        .spawn_blocking_named("process_metrics", move || metrics.snapshot())
        .await
        .map_err(|err| format!("performance measurements unavailable: {err}"))
}

pub async fn data(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers, true) {
        return response;
    }
    match snapshot(&state).await {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(message) => json_error_response(StatusCode::SERVICE_UNAVAILABLE, message),
    }
}

pub async fn page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(response) = authorize(&state, &headers, false) {
        return response;
    }
    let template = match load_template(&state, "templates/performance.html") {
        Ok(template) => template,
        Err(err) => return html_error(&state, StatusCode::INTERNAL_SERVER_ERROR, err),
    };
    let snapshot = match snapshot(&state).await {
        Ok(snapshot) => snapshot,
        Err(message) => return html_error(&state, StatusCode::SERVICE_UNAVAILABLE, message),
    };
    let current = snapshot.history.last();
    let cpu_percent = current.and_then(|sample| sample.cpu_percent);
    let cpu_status = current.map_or(CpuStatus::Unavailable, |sample| sample.cpu_status);
    let cpu = format_cpu(cpu_percent, snapshot.logical_cpus, cpu_status);
    let memory = current
        .and_then(|sample| sample.memory_bytes)
        .map(|value| format!("{:.1} MiB", value as f64 / 1_048_576.0))
        .unwrap_or_else(|| "Unavailable".into());
    let body = apply_template(
        template,
        &[
            ("cpu_value", cpu),
            ("memory_value", memory),
            ("pid", snapshot.pid.to_string()),
            ("uptime", format_uptime(snapshot.uptime_secs)),
            ("logical_cpus", snapshot.logical_cpus.to_string()),
            ("active_streams", snapshot.active_streams.to_string()),
            (
                "sent_value",
                format_rate(current.and_then(|sample| sample.sent_bytes_per_sec)),
            ),
            (
                "received_value",
                format_rate(current.and_then(|sample| sample.received_bytes_per_sec)),
            ),
        ],
    );
    render_admin_response(
        &state,
        StatusCode::OK,
        "Performance",
        &body,
        PageLayout::standard(),
    )
}

fn format_cpu(percent: Option<f64>, logical_cpus: usize, status: CpuStatus) -> String {
    if logical_cpus == 0 {
        return "Unavailable".into();
    }
    match (status, percent) {
        (CpuStatus::Available, Some(value)) if value.is_finite() && value >= 0.0 => {
            format!("{:.1}%", value / logical_cpus as f64)
        }
        (CpuStatus::Measuring, _) => "Measuring…".into(),
        _ => "Unavailable".into(),
    }
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes}m")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

fn format_rate(rate: Option<f64>) -> String {
    rate.map(|value| format!("{:.1} KiB/s", value / 1024.0))
        .unwrap_or_else(|| "Measuring…".into())
}

#[cfg(test)]
mod tests {
    use super::format_cpu;
    use crate::process_metrics::CpuStatus;

    #[test]
    fn cpu_display_uses_total_capacity_and_explicit_measurement_status() {
        use CpuStatus::{Available, Measuring, Unavailable};
        assert_eq!(format_cpu(Some(100.2), 4, Available), "25.1%");
        assert_eq!(format_cpu(Some(400.0), 4, Available), "100.0%");
        assert_eq!(format_cpu(Some(0.0), 4, Available), "0.0%");
        assert_eq!(format_cpu(None, 4, Measuring), "Measuring…");
        assert_eq!(format_cpu(None, 4, Unavailable), "Unavailable");
        assert_eq!(format_cpu(None, 4, Available), "Unavailable");
        assert_eq!(format_cpu(Some(100.2), 4, Unavailable), "Unavailable");
        assert_eq!(format_cpu(None, 0, Measuring), "Unavailable");
        for invalid in [-1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(format_cpu(Some(invalid), 4, Available), "Unavailable");
        }
    }
}
