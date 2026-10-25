use crate::api_config::ApiConfig;
use crate::pb;

pub enum AcmeKeyLookup {
    Found(String),
    Missing,
    RpcError(String),
}

pub async fn find_acme_key(api_config: &ApiConfig, token: &str, host: &str) -> AcmeKeyLookup {
    let client = match crate::rpc::client::SharedRpcClient::get(api_config).await {
        Ok(s) => s.as_rpc_client(),
        Err(err) => return AcmeKeyLookup::RpcError(err.to_string()),
    };
    let mut service = client.acme_service();

    let mut request = tonic::Request::new(pb::FindAcmeAuthenticationKeyWithTokenRequest {
        token: token.to_string(),
    });
    let hostname = match host.parse() {
        Ok(value) => value,
        Err(err) => return AcmeKeyLookup::RpcError(format!("invalid ACME hostname: {err}")),
    };
    request.metadata_mut().insert("x-zelvik-acme-host", hostname);
    match service
        .find_acme_authentication_key_with_token(request)
        .await
    {
        Ok(resp) => {
            let key = resp.into_inner().key;
            if key.is_empty() {
                AcmeKeyLookup::Missing
            } else {
                AcmeKeyLookup::Found(key)
            }
        }
        Err(err) => AcmeKeyLookup::RpcError(err.to_string()),
    }
}
