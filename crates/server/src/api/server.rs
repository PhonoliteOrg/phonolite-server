use axum::{extract::State, Json};

use crate::state::{AppState, ServerPortsResponse};

pub async fn get_ports(State(state): State<AppState>) -> Json<ServerPortsResponse> {
    let config = state.config.read();
    Json(ServerPortsResponse {
        http_port: config.port,
        quic_port: config.quic_port,
        quic_enabled: config.quic_enabled,
    })
}
