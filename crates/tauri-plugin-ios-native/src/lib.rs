use tauri::{
    plugin::{Builder, TauriPlugin},
    Runtime,
};

#[cfg(target_os = "ios")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "ios")]
use tauri::{plugin::PluginHandle, Manager};

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_ios_native);

#[cfg(target_os = "ios")]
#[derive(Clone)]
pub struct IosNative<R: Runtime>(PluginHandle<R>);

#[cfg(target_os = "ios")]
pub trait IosNativeExt<R: Runtime> {
    fn ios_native(&self) -> &IosNative<R>;
}

#[cfg(target_os = "ios")]
impl<R: Runtime, T: Manager<R>> IosNativeExt<R> for T {
    fn ios_native(&self) -> &IosNative<R> {
        self.state::<IosNative<R>>().inner()
    }
}

#[cfg(target_os = "ios")]
#[derive(Debug, Eq, PartialEq)]
pub enum WebAuthenticationOutcome {
    Callback(String),
    Cancelled,
    Failed,
}

#[cfg(target_os = "ios")]
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebAuthenticationRequest<'a> {
    authorization_url: &'a str,
    callback_scheme: &'a str,
    prefers_ephemeral: bool,
}

#[cfg(target_os = "ios")]
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebAuthenticationResponse {
    status: String,
    callback_url: Option<String>,
}

#[cfg(target_os = "ios")]
#[derive(Serialize)]
struct OpenedFilesRequest { urls: Vec<String> }

#[cfg(target_os = "ios")]
impl<R: Runtime> IosNative<R> {
    pub async fn receive_opened_files(&self, urls: Vec<String>) {
        let _ = self.0.run_mobile_plugin_async::<()>(
            "receiveOpenedFiles", OpenedFilesRequest { urls },
        ).await;
    }

    pub async fn authenticate(
        &self,
        authorization_url: &str,
        callback_scheme: &str,
        prefers_ephemeral: bool,
    ) -> WebAuthenticationOutcome {
        let response = self
            .0
            .run_mobile_plugin_async::<WebAuthenticationResponse>(
                "authenticate",
                WebAuthenticationRequest {
                    authorization_url,
                    callback_scheme,
                    prefers_ephemeral,
                },
            )
            .await;
        match response {
            Ok(response) if response.status == "succeeded" => response
                .callback_url
                .filter(|url| !url.is_empty())
                .map(WebAuthenticationOutcome::Callback)
                .unwrap_or(WebAuthenticationOutcome::Failed),
            Ok(response) if response.status == "cancelled" => {
                WebAuthenticationOutcome::Cancelled
            }
            _ => WebAuthenticationOutcome::Failed,
        }
    }

    pub async fn cancel_authentication(&self) {
        let _ = self
            .0
            .run_mobile_plugin_async::<()>("cancelAuthentication", ())
            .await;
    }
}

pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("ios-native")
        .setup(|app, api| {
            #[cfg(target_os = "ios")]
            {
                let handle = api.register_ios_plugin(init_plugin_ios_native)?;
                app.manage(IosNative(handle));
            }
            #[cfg(not(target_os = "ios"))]
            let _ = (app, api);
            Ok(())
        })
        .build()
}
