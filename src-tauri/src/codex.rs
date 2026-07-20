use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Child;
#[cfg(any(not(target_os = "windows"), test))]
use std::process::{Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use tungstenite::{Message, WebSocket, client};

#[cfg(target_os = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

#[cfg(all(test, target_os = "macos"))]
use tungstenite::connect;

#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt;

use crate::{compatibility, theme};

#[cfg(target_os = "macos")]
const CODEX_BUNDLE_ID: &str = "com.openai.codex";
#[cfg(target_os = "windows")]
const CODEX_PACKAGE_NAME: &str = "OpenAI.Codex";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);
const THEME_CHANNEL_IO_TIMEOUT: Duration = Duration::from_secs(3);
const THEME_CHANNEL_READ_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STATUS_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const STATUS_PROBE_GRACE_PERIOD: Duration = Duration::from_secs(15);
const PAGE_LEASE_DURATION: Duration = Duration::from_secs(15);
pub(crate) const PAGE_LEASE_RENEW_INTERVAL: Duration = Duration::from_secs(5);
const MAX_THEME_ASSET_CONNECTIONS: usize = 100;
const PAGE_RESTORE_THEME_FUNCTION: &str = r#"(expectedSessionId = null, closeWindow = false, revokeAssets = true) => {
  const runtime = window[runtimeKey];
  if (expectedSessionId !== null && runtime?.sessionId !== expectedSessionId) return false;
  runtime?.observer?.disconnect();
  runtime?.schemeObserver?.disconnect();
  runtime?.composerLayoutObserver?.disconnect();
  if (runtime?.colorSchemeMedia && runtime?.syncColorScheme) {
    runtime.colorSchemeMedia.removeEventListener('change', runtime.syncColorScheme);
  }
  const eventTarget = runtime?.eventTarget ?? runtime?.root;
  if (eventTarget && runtime?.handleInput) {
    eventTarget.removeEventListener('input', runtime.handleInput, true);
  }
  if (eventTarget && runtime?.handleNavigation) {
    eventTarget.removeEventListener('click', runtime.handleNavigation, true);
  }
  if (runtime?.handleResize) {
    window.removeEventListener('resize', runtime.handleResize);
  }
  if (runtime?.frame) cancelAnimationFrame(runtime.frame);
  if (runtime?.homeFrame) cancelAnimationFrame(runtime.homeFrame);
  if (runtime?.contentFrame) cancelAnimationFrame(runtime.contentFrame);
  if (runtime?.metricsFrame) cancelAnimationFrame(runtime.metricsFrame);
  if (runtime?.resizeFrame) cancelAnimationFrame(runtime.resizeFrame);
  if (runtime?.leaseTimer) clearInterval(runtime.leaseTimer);
  if (revokeAssets) runtime?.revokeAssets?.();
  if (window[runtimeKey] === runtime) delete window[runtimeKey];
  document.querySelectorAll('[data-ct-home-prompt-native]').forEach(node => {
    const display = node.getAttribute('data-ct-home-prompt-display');
    const priority = node.getAttribute('data-ct-home-prompt-display-priority') ?? '';
    if (display === null) node.style.removeProperty('display');
    else node.style.setProperty('display', display, priority);
    node.removeAttribute('data-ct-home-prompt-native');
    node.removeAttribute('data-ct-home-prompt-display');
    node.removeAttribute('data-ct-home-prompt-display-priority');
  });
  document.getElementById('codex-theme-runtime-style')?.remove();
  document.documentElement?.removeAttribute('data-ct-theme');
  document.documentElement?.removeAttribute('data-ct-view');
  document.documentElement?.removeAttribute('data-ct-color-scheme');
  document.documentElement?.style.removeProperty('--ct-home-card-count');
  document.documentElement?.style.removeProperty('--ct-titlebar-safe-top');
  document.querySelectorAll('[data-ct-slot="conversation.stage"]').forEach(node => {
    node.style.removeProperty('--ct-conversation-banner-clearance');
    node.style.removeProperty('--ct-conversation-summary-width');
    node.style.removeProperty('--ct-conversation-content-left');
    node.style.removeProperty('--ct-conversation-content-width');
    node.style.removeProperty('--ct-conversation-header-safe-top');
  });
  document.querySelectorAll('[data-ct-sidebar-footer-height]').forEach(node => {
    const original = node.getAttribute('data-ct-sidebar-footer-height');
    if (original) node.style.setProperty('--sidebar-footer-height', original);
    else node.style.removeProperty('--sidebar-footer-height');
    node.removeAttribute('data-ct-sidebar-footer-height');
  });
  document.querySelectorAll('[data-ct-mount]').forEach(node => node.remove());
  document.querySelectorAll('[data-ct-slot]').forEach(node => node.removeAttribute('data-ct-slot'));
  document.querySelectorAll('[data-ct-composer-layout]')
    .forEach(node => node.removeAttribute('data-ct-composer-layout'));
  document.querySelectorAll('[data-ct-workspace-panel]')
    .forEach(node => node.removeAttribute('data-ct-workspace-panel'));
  document.querySelectorAll('[data-ct-workspace-panel-region]')
    .forEach(node => node.removeAttribute('data-ct-workspace-panel-region'));
  document.querySelectorAll('[data-ct-native-titlebar]')
    .forEach(node => node.removeAttribute('data-ct-native-titlebar'));
  const removed = !document.getElementById('codex-theme-runtime-style')
    && !document.documentElement?.hasAttribute('data-ct-theme')
    && !document.documentElement?.hasAttribute('data-ct-color-scheme')
    && !document.querySelector('[data-ct-slot]')
    && !document.querySelector('[data-ct-mount]');
  if (closeWindow) setTimeout(() => window.close(), 0);
  return removed;
}"#;
const PAGE_LEASE_CONTROLLER_FUNCTION: &str = r#"(runtimeKey, runtime, pageLeaseMilliseconds) => {
  runtime.leaseExpiresAt = Math.min(
    Date.now() + pageLeaseMilliseconds,
    runtime.hardExpiresAt ?? Number.POSITIVE_INFINITY
  );
  runtime.syncThemeStatus?.();
  runtime.leaseTimer = setInterval(() => {
    if (window[runtimeKey] !== runtime) {
      clearInterval(runtime.leaseTimer);
      return;
    }
    const now = Date.now();
    runtime.syncThemeStatus?.();
    if (now >= runtime.leaseExpiresAt || (runtime.hardExpiresAt && now >= runtime.hardExpiresAt)) {
      runtime.restoreTheme(runtime.sessionId, true);
    }
  }, 1000);
}"#;
const PLATFORM_RUNTIME_CSS: &str = r#"
[data-ct-slot="app.shell"] {
  position: relative !important;
  isolation: isolate !important;
}
[data-ct-mount="app.background"] {
  position: absolute !important;
  z-index: -1 !important;
  inset: 0 !important;
  overflow: hidden !important;
  pointer-events: none !important;
}
[data-ct-mount="app.background"] > img {
  display: block !important;
  width: 100% !important;
  height: 100% !important;
}
:where([data-ct-slot="titlebar"]) {
  border: 0 !important;
  background: transparent !important;
  backdrop-filter: none !important;
  -webkit-backdrop-filter: none !important;
  box-shadow: none !important;
}
:where([data-ct-slot="titlebar"])::before,
:where([data-ct-slot="titlebar"])::after {
  display: none !important;
  border: 0 !important;
  background: transparent !important;
  box-shadow: none !important;
  content: none !important;
}
:where([data-ct-slot="sidebar"])::after {
  display: none !important;
  content: none !important;
}
:where([data-ct-slot="settings.sidebar"])::after {
  display: none !important;
  background: transparent !important;
  content: none !important;
}
:where([data-ct-slot="sidebar.resize.indicator"]) {
  display: none !important;
  opacity: 0 !important;
}
:where([data-ct-slot="main"]) {
  box-shadow: none !important;
}
:where([data-ct-slot="main.fade"]) {
  display: none !important;
}
:where([data-ct-slot="main.content.frame"]) {
  border: 0 !important;
  box-shadow: none !important;
}
:where([data-ct-slot="main.content.frame"])::before,
:where([data-ct-slot="main.content.frame"])::after {
  display: none !important;
  border: 0 !important;
  box-shadow: none !important;
  content: none !important;
}
[data-ct-slot="composer.context"] {
  opacity: 1 !important;
  backdrop-filter: none !important;
  -webkit-backdrop-filter: none !important;
}
[data-ct-slot="home.card"] {
  position: relative !important;
  isolation: isolate !important;
}
[data-ct-mount="home.card.background"] {
  position: absolute !important;
  z-index: -1 !important;
  inset: 0 !important;
  overflow: hidden !important;
  border-radius: inherit !important;
  pointer-events: none !important;
}
:root[data-ct-view="home-compact"] [data-ct-slot="home.prompt"],
:root[data-ct-view="home-compact"] [data-ct-slot="home.cards"] {
  display: none !important;
}
:root[data-ct-view="home-compact"] [data-ct-slot="home.layout"] {
  display: flex !important;
  flex-direction: column !important;
  width: 100% !important;
  max-width: none !important;
  min-height: 100% !important;
  align-self: stretch !important;
}
:root[data-ct-view="home-compact"] [data-ct-slot="home.stage"] {
  display: none !important;
}
:root[data-ct-view="home-compact"] [data-ct-slot="conversation.banner"] {
  margin-right: auto !important;
  margin-left: auto !important;
  align-self: center !important;
}
:root[data-ct-view="home-compact"] [data-ct-slot="composer.region"] {
  flex: 1 0 0% !important;
  min-width: 0 !important;
  justify-content: flex-end !important;
}
:root[data-ct-view="home"] [data-ct-slot="home.stage"] {
  display: flex !important;
  flex-direction: column !important;
  align-items: stretch !important;
  justify-content: center !important;
  width: 100% !important;
  padding-top: var(--ct-home-hero-top, 0px) !important;
}
:root[data-ct-view="home"] [data-ct-mount="home.hero"] {
  position: relative !important;
  top: auto !important;
  right: auto !important;
  bottom: auto !important;
  left: auto !important;
  box-sizing: border-box !important;
  flex: 0 0 auto !important;
  width: 100% !important;
  max-width: var(--ct-home-hero-max-width, 1080px) !important;
  margin-right: auto !important;
  margin-bottom: var(--ct-home-hero-gap, 24px) !important;
  margin-left: auto !important;
  transform: none !important;
}
[data-ct-slot="home.hero.viewport"] {
  position: relative !important;
  display: grid !important;
  grid-column: 1 / -1 !important;
  grid-row: 1 / -1 !important;
  grid-template-columns: inherit !important;
  grid-template-rows: inherit !important;
  grid-template-areas: inherit !important;
  align-items: inherit !important;
  justify-items: inherit !important;
  box-sizing: border-box !important;
  width: 100% !important;
  height: 100% !important;
  min-height: inherit !important;
  overflow: var(--ct-home-hero-viewport-overflow, hidden) !important;
  border-radius: inherit !important;
  pointer-events: none !important;
}
[data-ct-workspace-panel-region] {
  z-index: var(--ct-workspace-panel-z-index, 35) !important;
  isolation: isolate !important;
}
[data-ct-slot="home.hero"],
[data-ct-slot="conversation.banner"] {
  overflow: visible !important;
}
[data-ct-slot="conversation.banner"] {
  flex: 0 0 auto !important;
}
[data-ct-slot="conversation.stage"] {
  position: relative !important;
  display: flex !important;
  flex-direction: column !important;
  min-width: 0 !important;
  min-height: 0 !important;
  border: 0 !important;
  box-shadow: none !important;
}
[data-ct-slot="conversation.header"] {
  display: flex !important;
  flex-direction: column !important;
  box-sizing: border-box !important;
  flex: 0 0 auto !important;
  width: 100% !important;
  min-width: 0 !important;
  padding-top: var(--ct-conversation-header-safe-top, 0px) !important;
}
[data-ct-slot="conversation.header.content"] {
  display: flex !important;
  box-sizing: border-box !important;
  width: var(--ct-conversation-content-width, 100%) !important;
  max-width: none !important;
  min-width: 0 !important;
  margin-right: 0 !important;
  margin-left: var(--ct-conversation-content-left, 0px) !important;
  padding-inline: 0 !important;
}
[data-ct-slot="conversation.header.content"] > [data-ct-slot="conversation.banner"] {
  width: 100% !important;
  max-width: 100% !important;
  margin-inline: 0 !important;
  align-self: stretch !important;
}
[data-ct-slot="conversation.viewport"] {
  position: relative !important;
  flex: 1 1 0% !important;
  width: 100% !important;
  height: auto !important;
  min-width: 0 !important;
  min-height: 0 !important;
  overflow: hidden !important;
  border: 0 !important;
  box-shadow: none !important;
}
[data-ct-slot="composer.backdrop"] {
  display: none !important;
}
:root[data-ct-theme] [data-ct-slot="composer.editor"][data-ct-composer-layout="compact"][contenteditable="true"] {
  box-sizing: border-box !important;
  min-height: 1.25rem !important;
  padding-block: 0 !important;
  line-height: 1.25rem !important;
}
:root[data-ct-theme] [data-ct-slot="composer.editor"][data-ct-composer-layout="compact"][contenteditable="true"] > p {
  margin-block: 0 !important;
}
:where([data-ct-slot="conversation.stage"], [data-ct-slot="conversation.viewport"])::before,
:where([data-ct-slot="conversation.stage"], [data-ct-slot="conversation.viewport"])::after {
  border: 0 !important;
  box-shadow: none !important;
}
[data-ct-slot="home.hero.media"],
[data-ct-slot="conversation.banner.media"] {
  overflow: hidden !important;
  border-radius: inherit !important;
}
[data-ct-slot="home.hero.media"] {
  overflow: var(--ct-home-hero-media-overflow, hidden) !important;
}
[data-ct-slot="home.hero.foreground"] {
  position: absolute !important;
  z-index: var(--ct-home-hero-foreground-z-index, var(--ct-banner-foreground-z-index, 3)) !important;
  right: var(--ct-home-hero-foreground-right, var(--ct-banner-foreground-right, 4%)) !important;
  bottom: var(--ct-home-hero-foreground-bottom, var(--ct-banner-foreground-bottom, 0px)) !important;
  display: block !important;
  width: var(--ct-home-hero-foreground-width, var(--ct-banner-foreground-width, min(34%, 360px))) !important;
  height: auto !important;
  overflow: visible !important;
  pointer-events: none !important;
}
[data-ct-slot="conversation.banner.foreground"] {
  position: absolute !important;
  z-index: var(--ct-conversation-banner-foreground-z-index, var(--ct-banner-foreground-z-index, 3)) !important;
  right: var(--ct-conversation-banner-foreground-right, var(--ct-banner-foreground-right, 4%)) !important;
  bottom: var(--ct-conversation-banner-foreground-bottom, var(--ct-banner-foreground-bottom, 0px)) !important;
  display: block !important;
  width: var(--ct-conversation-banner-foreground-width, var(--ct-banner-foreground-width, min(34%, 360px))) !important;
  height: auto !important;
  overflow: visible !important;
  pointer-events: none !important;
}
[data-ct-slot="home.hero.foreground.asset"],
[data-ct-slot="conversation.banner.foreground.asset"] {
  display: block !important;
  height: auto !important;
  object-fit: contain !important;
  object-position: center bottom !important;
}
[data-ct-slot="home.hero.foreground.asset"] {
  width: 100% !important;
}
[data-ct-slot="conversation.banner.foreground.asset"] {
  width: auto !important;
  max-width: 100% !important;
  height: auto !important;
  max-height: var(--ct-conversation-banner-foreground-safe-height, none) !important;
  margin-left: auto !important;
}
[data-ct-slot="sidebar.header"] {
  position: relative !important;
  isolation: isolate !important;
}
[data-ct-mount="sidebar.header.background"] {
  position: absolute !important;
  z-index: -1 !important;
  inset: 0 !important;
  overflow: hidden !important;
  pointer-events: none !important;
}
[data-ct-mount="sidebar.header.background"] > img {
  display: block !important;
  width: 100% !important;
  height: 100% !important;
}
[data-ct-slot="sidebar.footer"] {
  box-sizing: border-box !important;
  min-height: 78px !important;
  padding-top: 32px !important;
}
[data-ct-mount="sidebar.footer.brand"] {
  position: absolute !important;
  z-index: 2 !important;
  top: 0 !important;
  right: 12px !important;
  left: 12px !important;
  display: flex !important;
  box-sizing: border-box !important;
  align-items: center !important;
  justify-content: space-between !important;
  gap: 8px !important;
  height: 32px !important;
  min-height: 32px !important;
  padding: 0 !important;
  border: 0 !important;
  border-bottom: 0 !important;
  box-shadow: none !important;
  color: var(--ct-text-secondary, currentColor);
  font-size: 11px;
  line-height: 18px !important;
  letter-spacing: 0.02em;
  pointer-events: none !important;
}
[data-ct-mount="sidebar.footer.brand"]::before,
[data-ct-mount="sidebar.footer.brand"]::after {
  display: none !important;
  content: none !important;
}
[data-ct-slot="sidebar.footer.brand.label"] {
  display: inline-flex !important;
  align-self: stretch !important;
  align-items: center !important;
  min-width: 0;
  margin-right: auto;
  font-size: 14px !important;
  font-weight: 650;
  line-height: 18px !important;
  letter-spacing: 0.01em;
}
[data-ct-slot="sidebar.footer.brand.timer"],
[data-ct-slot="sidebar.footer.brand.version"] {
  display: inline-flex !important;
  align-self: stretch !important;
  align-items: center !important;
  justify-content: center !important;
  margin-left: auto;
  color: var(--ct-color-accent, currentColor);
  font-size: 11px;
  font-variant-numeric: tabular-nums;
  font-weight: 650;
  letter-spacing: 0.04em;
}
[data-ct-slot="sidebar.footer.brand.pro"] {
  display: inline-flex !important;
  align-items: center !important;
  justify-content: center !important;
  margin-left: auto;
  padding: 2px 5px;
  border: 1px solid var(--ct-border-strong, currentColor);
  border-radius: 999px;
  background: var(--ct-color-accent-soft, transparent);
  color: var(--ct-color-accent, currentColor);
  font-size: 9px;
  font-weight: 700;
  letter-spacing: 0.04em;
}
"#;
#[cfg(target_os = "windows")]
const WINDOWS_PLATFORM_RUNTIME_CSS: &str = r#"
:where([data-ct-native-titlebar]) {
  border: 0 !important;
  background: transparent !important;
  backdrop-filter: none !important;
  -webkit-backdrop-filter: none !important;
  box-shadow: none !important;
}
:where([data-ct-native-titlebar])::before,
:where([data-ct-native-titlebar])::after {
  display: none !important;
  border: 0 !important;
  background: transparent !important;
  box-shadow: none !important;
  content: none !important;
}
:root[data-ct-view="home"] [data-ct-slot="home.stage"] {
  justify-content: flex-start !important;
  padding-top: 0 !important;
}
:where([data-ct-slot="settings.content"], [data-ct-slot="settings.surface"], [data-ct-slot="settings.frame"], [data-ct-slot="settings.canvas"], [data-ct-slot="settings.body"]) {
  border: 0 !important;
  border-radius: 0 !important;
  background: transparent !important;
  box-shadow: none !important;
}
:where([data-ct-slot="settings.content"], [data-ct-slot="settings.surface"], [data-ct-slot="settings.frame"], [data-ct-slot="settings.canvas"], [data-ct-slot="settings.body"])::before,
:where([data-ct-slot="settings.content"], [data-ct-slot="settings.surface"], [data-ct-slot="settings.frame"], [data-ct-slot="settings.canvas"], [data-ct-slot="settings.body"])::after {
  border: 0 !important;
  background: transparent !important;
  box-shadow: none !important;
}
:where([data-ct-slot="settings.frame"]) {
  margin-top: 0 !important;
}
:where([data-ct-slot="settings.toolbar"]) {
  display: none !important;
}
"#;

fn platform_runtime_css() -> String {
    #[cfg(target_os = "windows")]
    {
        format!("{PLATFORM_RUNTIME_CSS}\n{WINDOWS_PLATFORM_RUNTIME_CSS}")
    }
    #[cfg(not(target_os = "windows"))]
    {
        PLATFORM_RUNTIME_CSS.to_owned()
    }
}
#[cfg(any(target_os = "macos", target_os = "windows"))]
const PROCESS_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);
const REQUIRED_GLOBAL_STARTUP_SLOTS: &[&str] = &["app.shell", "titlebar", "sidebar", "main"];
const REQUIRED_COMPOSER_STARTUP_SLOTS: &[&str] = &[
    "composer",
    "composer.editor",
    "composer.submit",
    "composer.submit.icon",
];
const REQUIRED_HOME_STARTUP_SLOTS: &[&str] = &[
    "home.hero",
    "home.hero.viewport",
    "home.hero.copy",
    "home.hero.media",
    "home.hero.divider",
    "home.content.region",
    "home.stage",
    "home.prompt",
    "home.prompt.title",
    "home.cards",
    "home.cards.layout",
    "home.cards.grid",
    "home.card",
    "home.card.icon",
    "home.card.label",
];
const REQUIRED_COMPACT_HOME_STARTUP_SLOTS: &[&str] = &["home.layout", "composer.region"];
const REQUIRED_CONVERSATION_STARTUP_SLOTS: &[&str] = &[
    "conversation.stage",
    "conversation.viewport",
    "conversation",
];
const REQUIRED_STARTUP_VIEW: &str = "view:home|home-compact|conversation";

#[derive(Debug)]
pub struct CodexError(String);

impl fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CodexError {}

impl From<std::io::Error> for CodexError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexInstallation {
    app_name: String,
    path: PathBuf,
    executable: PathBuf,
    bundle_id: String,
    #[cfg(target_os = "windows")]
    app_user_model_id: String,
    version: String,
}

impl CodexInstallation {
    pub(crate) fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SmokeTestReport {
    app_version: String,
    browser_version: String,
    port: u16,
    target_title: String,
    target_url: String,
    loopback_only: bool,
    probe_applied: bool,
    probe_removed: bool,
    duration_ms: u128,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevToolsVersion {
    #[serde(rename = "Browser")]
    browser: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevToolsTarget {
    title: String,
    #[serde(rename = "type")]
    target_type: String,
    url: String,
    web_socket_debugger_url: String,
}

#[cfg_attr(all(target_os = "windows", not(test)), allow(dead_code))]
enum IsolatedProcess {
    Native {
        child: Child,
        #[cfg(target_os = "macos")]
        process_group_id: Option<i32>,
    },
    #[cfg(target_os = "windows")]
    WindowsStore(WindowsStoreProcess),
}

#[cfg(target_os = "windows")]
struct WindowsStoreProcess {
    id: u32,
    handle: OwnedHandle,
}

struct TestInstance {
    process: IsolatedProcess,
    _profile: TempDir,
}

struct ThemeSession {
    id: u64,
    instance: TestInstance,
    _asset_server: Option<ThemeAssetServer>,
    port: u16,
    deadline: Option<Instant>,
    target_missing_since: Option<Instant>,
    report: ThemePreviewReport,
}

struct ThemeAssetServer {
    address: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<Vec<thread::JoinHandle<()>>>>,
}

impl ThemeAssetServer {
    fn start(
        assets: Vec<theme::ThemeRuntimeAsset>,
    ) -> Result<(Self, HashMap<String, String>, String), CodexError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let mut token_bytes = [0_u8; 32];
        OsRng.fill_bytes(&mut token_bytes);
        let token = hex::encode(token_bytes);
        let mut indexed_assets = assets;
        indexed_assets.sort_by(|left, right| left.path.cmp(&right.path));
        let mut urls = HashMap::with_capacity(indexed_assets.len());
        let mut routes = HashMap::with_capacity(indexed_assets.len());
        for (index, asset) in indexed_assets.into_iter().enumerate() {
            let route = format!("/{token}/{index}");
            urls.insert(asset.path.clone(), format!("http://{address}{route}"));
            routes.insert(route, asset);
        }
        let revoke_url = format!("http://{address}/{token}/revoke");
        let revoke_path = format!("/{token}/revoke");
        let routes = Arc::new(routes);
        let shutdown = Arc::new(AtomicBool::new(false));
        let server_shutdown = shutdown.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let server_stopped = stopped.clone();
        let thread = thread::Builder::new()
            .name("retheme-assets".into())
            .spawn(move || {
                let mut connections = Vec::new();
                while !server_shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            connections.retain(|connection: &thread::JoinHandle<()>| {
                                !connection.is_finished()
                            });
                            if connections.len() >= MAX_THEME_ASSET_CONNECTIONS {
                                let _ = write_theme_asset_error(
                                    &mut stream,
                                    503,
                                    "Service Unavailable",
                                );
                                continue;
                            }
                            let routes = routes.clone();
                            let revoke_path = revoke_path.clone();
                            let shutdown = server_shutdown.clone();
                            let stopped = server_stopped.clone();
                            if let Ok(connection) = thread::Builder::new()
                                .name("retheme-asset-request".into())
                                .spawn(move || {
                                    let _ = serve_theme_asset_request(
                                        &mut stream,
                                        address,
                                        &revoke_path,
                                        &routes,
                                        &shutdown,
                                        &stopped,
                                    );
                                })
                            {
                                connections.push(connection);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
                drop(listener);
                server_stopped.store(true, Ordering::Release);
                connections
            })?;
        Ok((
            Self {
                address,
                shutdown,
                thread: Some(thread),
            },
            urls,
            revoke_url,
        ))
    }
}

impl Drop for ThemeAssetServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_millis(100));
        if let Some(thread) = self.thread.take()
            && let Ok(connections) = thread.join()
        {
            for connection in connections {
                let _ = connection.join();
            }
        }
    }
}

fn serve_theme_asset_request(
    stream: &mut TcpStream,
    address: SocketAddr,
    revoke_path: &str,
    routes: &HashMap<String, theme::ThemeRuntimeAsset>,
    shutdown: &AtomicBool,
    stopped: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let _ = stream.set_nodelay(true);
    let mut request = [0_u8; 8192];
    let mut length = 0;
    let mut header_complete = false;
    while length < request.len() {
        let read = stream.read(&mut request[length..])?;
        if read == 0 {
            return Ok(());
        }
        length += read;
        if request[..length]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            header_complete = true;
            break;
        }
    }
    if !header_complete {
        return write_theme_asset_error(stream, 431, "Request Header Fields Too Large");
    }
    let request = match std::str::from_utf8(&request[..length]) {
        Ok(request) => request,
        Err(_) => return write_theme_asset_error(stream, 400, "Bad Request"),
    };
    let mut lines = request.split("\r\n");
    let Some((method, path, version)) = lines.next().and_then(|line| {
        let mut parts = line.split_whitespace();
        Some((parts.next()?, parts.next()?, parts.next()?))
    }) else {
        return write_theme_asset_error(stream, 400, "Bad Request");
    };
    if !matches!(method, "GET" | "HEAD") || version != "HTTP/1.1" {
        return write_theme_asset_error(stream, 404, "Not Found");
    }
    let expected_host = address.to_string();
    let valid_host = lines.any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("host") && value.trim() == expected_host
        })
    });
    if valid_host && path == revoke_path {
        shutdown.store(true, Ordering::Release);
        while !stopped.load(Ordering::Acquire) {
            thread::yield_now();
        }
        return write_theme_asset_error(stream, 204, "No Content");
    }
    let Some(asset) = valid_host.then(|| routes.get(path)).flatten() else {
        return write_theme_asset_error(stream, 404, "Not Found");
    };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: private, no-store\r\nCross-Origin-Resource-Policy: cross-origin\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src data:\r\nConnection: close\r\n\r\n",
        asset.mime,
        asset.source.len(),
    )?;
    if method == "GET" {
        for chunk in asset.source.chunks(64 * 1024) {
            stream.write_all(chunk)?;
        }
    }
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn write_theme_asset_error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

#[derive(Clone)]
pub struct ThemeRuntime {
    session: std::sync::Arc<std::sync::Mutex<Option<ThemeSession>>>,
    next_session_id: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Default for ThemeRuntime {
    fn default() -> Self {
        Self {
            session: Default::default(),
            next_session_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }
}

impl ThemeRuntime {
    pub fn current_preview(&self) -> Result<Option<ThemePreviewReport>, CodexError> {
        let mut session = self
            .session
            .lock()
            .map_err(|_| CodexError("主题会话状态已损坏".into()))?;
        let inactive = match session.as_mut() {
            Some(session) => {
                let expired_or_exited = session
                    .deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
                    || session.instance.process.has_exited()?;
                if expired_or_exited {
                    true
                } else {
                    match find_codex_target(session.port, STATUS_PROBE_TIMEOUT) {
                        Ok(Some(_)) => {
                            session.target_missing_since = None;
                            false
                        }
                        Ok(None) | Err(_) => {
                            let missing_since = session
                                .target_missing_since
                                .get_or_insert_with(Instant::now);
                            missing_since.elapsed() >= STATUS_PROBE_GRACE_PERIOD
                        }
                    }
                }
            }
            None => return Ok(None),
        };
        if inactive {
            let inactive_session = session.take();
            drop(session);
            drop(inactive_session);
            return Ok(None);
        }
        Ok(session.as_ref().map(|session| session.report.clone()))
    }

    pub fn current_theme_id(&self) -> Option<String> {
        self.current_preview()
            .ok()
            .flatten()
            .map(|report| report.theme_id)
    }

    pub fn renew_page_lease(&self) -> Result<bool, CodexError> {
        let (session_id, port) = {
            let session = self
                .session
                .lock()
                .map_err(|_| CodexError("主题会话状态已损坏".into()))?;
            let Some(session) = session.as_ref() else {
                return Ok(false);
            };
            (session.id, session.port)
        };
        let Some(target) = find_codex_target(port, STATUS_PROBE_TIMEOUT)? else {
            return Ok(false);
        };
        renew_theme_lease(&target.web_socket_debugger_url, session_id)
    }

    fn next_session_id(&self) -> u64 {
        self.next_session_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum ThemePreviewSource {
    Installed,
    LocalDevelopment,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemePreviewReport {
    theme_id: String,
    theme: theme::ThemeSummary,
    source: ThemePreviewSource,
    expires_at: Option<u64>,
    app_version: String,
    port: u16,
    applied_slots: Vec<String>,
    loopback_only: bool,
}

impl Drop for TestInstance {
    fn drop(&mut self) {
        self.process.terminate();
    }
}

impl IsolatedProcess {
    fn id(&self) -> u32 {
        match self {
            Self::Native { child, .. } => child.id(),
            #[cfg(target_os = "windows")]
            Self::WindowsStore(process) => process.id,
        }
    }

    fn has_exited(&mut self) -> Result<bool, CodexError> {
        match self {
            Self::Native { child, .. } => Ok(child.try_wait()?.is_some()),
            #[cfg(target_os = "windows")]
            Self::WindowsStore(process) => process.has_exited(),
        }
    }

    fn terminate(&mut self) {
        match self {
            Self::Native {
                child,
                #[cfg(target_os = "macos")]
                process_group_id,
            } => {
                #[cfg(target_os = "macos")]
                if let Some(process_group_id) = process_group_id {
                    signal_process_group(*process_group_id, libc::SIGTERM);
                    let started = Instant::now();
                    while process_group_exists(*process_group_id)
                        && started.elapsed() < PROCESS_SHUTDOWN_TIMEOUT
                    {
                        let _ = child.try_wait();
                        thread::sleep(Duration::from_millis(25));
                    }
                    if process_group_exists(*process_group_id) {
                        signal_process_group(*process_group_id, libc::SIGKILL);
                    }
                } else {
                    let _ = child.kill();
                }
                #[cfg(not(target_os = "macos"))]
                let _ = child.kill();
                let _ = child.wait();
            }
            #[cfg(target_os = "windows")]
            Self::WindowsStore(process) => process.terminate(),
        }
    }
}

#[cfg(target_os = "windows")]
impl WindowsStoreProcess {
    fn open(id: u32) -> Result<Self, CodexError> {
        use windows::Win32::System::Threading::{
            OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        };

        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                id,
            )
        }
        .map_err(|error| CodexError(format!("无法管理隔离 ChatGPT 进程 {id}：{error}")))?;
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Ok(Self { id, handle })
    }

    fn has_exited(&self) -> Result<bool, CodexError> {
        use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::WaitForSingleObject;

        let handle = HANDLE(self.handle.as_raw_handle());
        match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            status => Err(CodexError(format!(
                "无法读取隔离 ChatGPT 进程 {} 状态：等待结果 {}",
                self.id, status.0
            ))),
        }
    }

    fn terminate(&self) {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

        if self.has_exited().unwrap_or(true) {
            return;
        }
        let handle = HANDLE(self.handle.as_raw_handle());
        let _ = unsafe { TerminateProcess(handle, 0) };
        let _ = unsafe {
            WaitForSingleObject(
                handle,
                PROCESS_SHUTDOWN_TIMEOUT.as_millis().min(u32::MAX as u128) as u32,
            )
        };
    }
}

#[cfg(target_os = "macos")]
fn signal_process_group(process_group_id: i32, signal: i32) {
    unsafe {
        libc::kill(-process_group_id, signal);
    }
}

#[cfg(target_os = "macos")]
fn process_group_exists(process_group_id: i32) -> bool {
    unsafe { libc::kill(-process_group_id, 0) == 0 }
}

pub fn detect() -> Result<CodexInstallation, CodexError> {
    #[cfg(target_os = "macos")]
    {
        for app_path in ["/Applications/ChatGPT.app", "/Applications/Codex.app"] {
            if let Ok(installation) = inspect_macos_app(Path::new(app_path)) {
                return Ok(installation);
            }
        }
        Err(CodexError("未在 /Applications 中找到 ChatGPT".into()))
    }

    #[cfg(target_os = "windows")]
    {
        inspect_windows_package()
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Err(CodexError("当前平台暂不支持 ChatGPT 检测".into()))
    }
}

#[cfg(target_os = "windows")]
fn inspect_windows_package() -> Result<CodexInstallation, CodexError> {
    use windows::Management::Deployment::PackageManager;
    use windows::core::HSTRING;

    let manager = PackageManager::new()
        .map_err(|error| CodexError(format!("无法访问 Windows 应用包管理器：{error}")))?;
    let packages = manager
        .FindPackagesByUserSecurityId(&HSTRING::new())
        .map_err(|error| CodexError(format!("无法读取当前用户的 Windows 应用包：{error}")))?;
    for package in packages {
        let id = package
            .Id()
            .map_err(|error| CodexError(format!("无法读取 Windows 应用包标识：{error}")))?;
        let name = id
            .Name()
            .map_err(|error| CodexError(format!("无法读取 Windows 应用包名称：{error}")))?
            .to_string();
        if name != CODEX_PACKAGE_NAME {
            continue;
        }
        let location = package
            .InstalledLocation()
            .and_then(|folder| folder.Path())
            .map_err(|error| CodexError(format!("无法读取 ChatGPT 安装目录：{error}")))?;
        let path = PathBuf::from(location.to_string());
        let executable = path.join("app").join("ChatGPT.exe");
        if !executable.is_file() {
            return Err(CodexError(format!(
                "ChatGPT 可执行文件不存在：{}",
                executable.display()
            )));
        }
        let package_version = id
            .Version()
            .map_err(|error| CodexError(format!("无法读取 ChatGPT 版本：{error}")))?;
        let family_name = id
            .FamilyName()
            .map_err(|error| CodexError(format!("无法读取 ChatGPT 包族：{error}")))?
            .to_string();
        let entries = package
            .GetAppListEntries()
            .map_err(|error| CodexError(format!("无法读取 ChatGPT 应用入口：{error}")))?;
        let app_user_model_id = (0..entries
            .Size()
            .map_err(|error| CodexError(format!("无法读取 ChatGPT 应用入口数量：{error}")))?)
            .filter_map(|index| entries.GetAt(index).ok())
            .filter_map(|entry| entry.AppUserModelId().ok())
            .map(|app_user_model_id| app_user_model_id.to_string())
            .find(|app_user_model_id| {
                app_user_model_id
                    .strip_prefix(&family_name)
                    .is_some_and(|application_id| application_id.starts_with('!'))
            })
            .ok_or_else(|| CodexError("ChatGPT 安装包没有可激活的应用入口".into()))?;
        return Ok(CodexInstallation {
            app_name: "ChatGPT".to_owned(),
            path,
            executable,
            bundle_id: family_name,
            app_user_model_id,
            version: format!(
                "{}.{}.{}.{}",
                package_version.Major,
                package_version.Minor,
                package_version.Build,
                package_version.Revision
            ),
        });
    }
    Err(CodexError("未找到 Microsoft Store 安装的 ChatGPT".into()))
}

#[cfg(target_os = "macos")]
fn inspect_macos_app(app_path: &Path) -> Result<CodexInstallation, CodexError> {
    let info_path = app_path.join("Contents/Info.plist");
    let info = plist::Value::from_file(&info_path)
        .map_err(|error| CodexError(format!("无法读取 {}：{error}", info_path.display())))?;
    let dictionary = info
        .as_dictionary()
        .ok_or_else(|| CodexError(format!("{} 不是有效的 Info.plist", info_path.display())))?;
    let bundle_id = plist_string(dictionary, "CFBundleIdentifier")?;
    if bundle_id != CODEX_BUNDLE_ID {
        return Err(CodexError(format!("{} 不是 ChatGPT", app_path.display())));
    }
    let executable_name = plist_string(dictionary, "CFBundleExecutable")?;
    let version = plist_string(dictionary, "CFBundleShortVersionString")?;
    let executable = app_path.join("Contents/MacOS").join(executable_name);
    if !executable.is_file() {
        return Err(CodexError(format!(
            "ChatGPT 可执行文件不存在：{}",
            executable.display()
        )));
    }

    Ok(CodexInstallation {
        app_name: "ChatGPT".to_owned(),
        path: app_path.to_path_buf(),
        executable,
        bundle_id: bundle_id.to_owned(),
        version: version.to_owned(),
    })
}

#[cfg(target_os = "macos")]
fn plist_string<'a>(dictionary: &'a plist::Dictionary, key: &str) -> Result<&'a str, CodexError> {
    dictionary
        .get(key)
        .and_then(plist::Value::as_string)
        .ok_or_else(|| CodexError(format!("Info.plist 缺少 {key}")))
}

pub fn run_smoke_test() -> Result<SmokeTestReport, CodexError> {
    let started_at = Instant::now();
    let installation = detect()?;
    let mut instance = start_isolated(&installation)?;
    let (port, _) = wait_for_devtools(instance._profile.path(), &mut instance.process)?;
    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let loopback_only = verify_loopback(socket, instance.process.id())?;
    let version: DevToolsVersion = get_json(port, "/json/version")?;
    let target = wait_for_codex_target(port, &mut instance.process)?;

    let (probe_applied, probe_removed) = test_injection(&target.web_socket_debugger_url)?;
    if !probe_applied || !probe_removed {
        return Err(CodexError("本地主题通道没有完成应用与撤销闭环".into()));
    }

    Ok(SmokeTestReport {
        app_version: installation.version,
        browser_version: version.browser,
        port,
        target_title: target.title,
        target_url: target.url,
        loopback_only,
        probe_applied,
        probe_removed,
        duration_ms: started_at.elapsed().as_millis(),
    })
}

fn normalize_theme_locale(locale: &str) -> &'static str {
    if locale.to_ascii_lowercase().starts_with("zh") {
        "zh-CN"
    } else {
        "en"
    }
}

pub fn start_theme_preview_until(
    runtime: &ThemeRuntime,
    themes: &theme::ThemeRepository,
    compatibility: &compatibility::CompatibilityRepository,
    theme_id: &str,
    expires_at: Option<u64>,
    has_pro: bool,
    locale: &str,
) -> Result<ThemePreviewReport, CodexError> {
    if runtime.current_preview()?.is_some() {
        return Err(CodexError("已有主题预览正在运行，请先恢复当前主题".into()));
    }

    let package = themes
        .load(theme_id)
        .map_err(|error| CodexError(error.to_string()))?;
    start_theme_package_preview(
        runtime,
        compatibility,
        package,
        ThemePreviewSource::Installed,
        expires_at,
        has_pro,
        locale,
    )
}

pub fn start_development_theme_preview(
    runtime: &ThemeRuntime,
    themes: &theme::ThemeRepository,
    compatibility: &compatibility::CompatibilityRepository,
    theme_path: &Path,
    duration: Option<Duration>,
    has_pro: bool,
    locale: &str,
) -> Result<ThemePreviewReport, CodexError> {
    if runtime.current_preview()?.is_some() {
        return Err(CodexError("已有主题预览正在运行，请先恢复当前主题".into()));
    }
    let package = themes
        .load_development(theme_path)
        .map_err(|error| CodexError(error.to_string()))?;
    let expires_at = duration.map(|duration| unix_time().saturating_add(duration.as_secs()));
    start_theme_package_preview(
        runtime,
        compatibility,
        package,
        ThemePreviewSource::LocalDevelopment,
        expires_at,
        has_pro,
        locale,
    )
}

fn start_theme_package_preview(
    runtime: &ThemeRuntime,
    compatibility: &compatibility::CompatibilityRepository,
    package: theme::ThemePackage,
    source: ThemePreviewSource,
    expires_at: Option<u64>,
    has_pro: bool,
    locale: &str,
) -> Result<ThemePreviewReport, CodexError> {
    let installation = detect()?;
    let adapter = compatibility.adapter(&installation.version);

    let mut instance = start_isolated(&installation)?;
    let (port, _) = wait_for_devtools(instance._profile.path(), &mut instance.process)?;
    let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let loopback_only = verify_loopback(socket, instance.process.id())?;
    let target = wait_for_codex_target(port, &mut instance.process)?;
    let session_id = runtime.next_session_id();
    let remaining = expires_at.map(remaining_until).transpose()?.flatten();
    let (asset_server, asset_urls, revoke_assets_url) =
        ThemeAssetServer::start(package.runtime_assets())?;
    let runtime_config = package
        .runtime_config_with_asset_urls(&asset_urls)
        .map_err(|error| CodexError(error.to_string()))?;
    let applied_slots = apply_theme(
        &target.web_socket_debugger_url,
        &package,
        ThemeApplication {
            runtime_config: &runtime_config,
            revoke_assets_url: &revoke_assets_url,
            adapter: &adapter,
            session_id,
            expires_at,
            has_pro,
            locale,
        },
    )?;

    let report = ThemePreviewReport {
        theme_id: package.id().to_owned(),
        theme: package.preview_summary(),
        source,
        expires_at,
        app_version: installation.version,
        port,
        applied_slots,
        loopback_only,
    };
    let mut session = runtime
        .session
        .lock()
        .map_err(|_| CodexError("主题会话状态已损坏".into()))?;
    if session.is_some() {
        let _ = remove_theme(&target.web_socket_debugger_url);
        return Err(CodexError("主题预览启动发生并发冲突".into()));
    }
    *session = Some(ThemeSession {
        id: session_id,
        instance,
        _asset_server: Some(asset_server),
        port,
        deadline: remaining.map(|duration| Instant::now() + duration),
        target_missing_since: None,
        report: report.clone(),
    });

    Ok(report)
}

fn remaining_until(expires_at: u64) -> Result<Option<Duration>, CodexError> {
    let remaining = expires_at.saturating_sub(unix_time());
    if remaining == 0 {
        return Err(CodexError("主题授权或试用已到期".into()));
    }
    Ok(Some(Duration::from_secs(remaining)))
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn stop_theme_preview(runtime: &ThemeRuntime) -> Result<bool, CodexError> {
    let mut state = runtime
        .session
        .lock()
        .map_err(|_| CodexError("主题会话状态已损坏".into()))?;
    let session = state.take();
    drop(state);
    let Some(mut session) = session else {
        return Ok(false);
    };

    if session.instance.process.has_exited()? {
        return Ok(true);
    }

    let result = match find_codex_target(session.port, STATUS_PROBE_TIMEOUT) {
        Ok(Some(target)) => remove_theme(&target.web_socket_debugger_url),
        Ok(None) | Err(_) => Ok(true),
    };
    drop(session);
    result
}

pub fn sync_theme_locale(runtime: &ThemeRuntime, locale: &str) -> Result<bool, CodexError> {
    let (session_id, port) = {
        let session = runtime
            .session
            .lock()
            .map_err(|_| CodexError("主题会话状态已损坏".into()))?;
        let Some(session) = session.as_ref() else {
            return Ok(false);
        };
        (session.id, session.port)
    };
    let Some(target) = find_codex_target(port, STATUS_PROBE_TIMEOUT)? else {
        return Ok(false);
    };
    let locale = serde_json::to_string(normalize_theme_locale(locale))
        .map_err(|error| CodexError(format!("主题语言无法编码：{error}")))?;
    let mut socket = connect_theme_channel(&target.web_socket_debugger_url)?;
    let expression = format!(
        r#"(() => {{
          const runtime = window.__codexThemeRuntime;
          if (!runtime || runtime.sessionId !== {session_id} || !runtime.syncLocale) return false;
          return runtime.syncLocale({locale});
        }})()"#
    );
    let synchronized = evaluate(&mut socket, 1, &expression)?;
    let _ = socket.close(None);
    Ok(synchronized)
}

#[cfg(test)]
fn stop_theme_preview_if_session(
    runtime: &ThemeRuntime,
    session_id: u64,
) -> Result<bool, CodexError> {
    let matches = runtime
        .session
        .lock()
        .map_err(|_| CodexError("主题会话状态已损坏".into()))?
        .as_ref()
        .is_some_and(|session| session.id == session_id);
    if !matches {
        return Ok(false);
    }
    stop_theme_preview(runtime)
}

struct ThemeApplication<'a> {
    runtime_config: &'a Value,
    revoke_assets_url: &'a str,
    adapter: &'a compatibility::CodexAdapter,
    session_id: u64,
    expires_at: Option<u64>,
    has_pro: bool,
    locale: &'a str,
}

fn apply_theme(
    websocket_url: &str,
    package: &theme::ThemePackage,
    application: ThemeApplication<'_>,
) -> Result<Vec<String>, CodexError> {
    let ThemeApplication {
        runtime_config,
        revoke_assets_url,
        adapter,
        session_id,
        expires_at,
        has_pro,
        locale,
    } = application;
    let mut socket = connect_theme_channel(websocket_url)?;
    set_page_csp_bypass(&mut socket, 1, true)?;
    let theme_id = serde_json::to_string(package.id())
        .map_err(|error| CodexError(format!("主题 ID 无法编码：{error}")))?;
    let css = serde_json::to_string(package.css())
        .map_err(|error| CodexError(format!("主题 CSS 无法编码：{error}")))?;
    let platform_css = serde_json::to_string(&platform_runtime_css())
        .map_err(|error| CodexError(format!("平台主题 CSS 无法编码：{error}")))?;
    let runtime_config = serde_json::to_string(runtime_config)
        .map_err(|error| CodexError(format!("主题运行配置无法编码：{error}")))?;
    let revoke_assets_url = serde_json::to_string(revoke_assets_url)
        .map_err(|error| CodexError(format!("主题资源回收地址无法编码：{error}")))?;
    let adapter_config = serde_json::to_string(adapter)
        .map_err(|error| CodexError(format!("ChatGPT 适配配置无法编码：{error}")))?;
    let theme_version = serde_json::to_string(package.version())
        .map_err(|error| CodexError(format!("主题版本无法编码：{error}")))?;
    let locale = serde_json::to_string(normalize_theme_locale(locale))
        .map_err(|error| CodexError(format!("主题语言无法编码：{error}")))?;
    let hard_expires_at =
        serde_json::to_string(&expires_at.map(|value| value.saturating_mul(1000)))
            .map_err(|error| CodexError(format!("主题到期时间无法编码：{error}")))?;
    let page_lease_milliseconds = PAGE_LEASE_DURATION.as_millis();
    let restore_theme_function = PAGE_RESTORE_THEME_FUNCTION;
    let lease_controller_function = PAGE_LEASE_CONTROLLER_FUNCTION;
    let expression = format!(
        r#"(() => {{
          const baseConfig = {runtime_config};
          let config = baseConfig;
          let requestedLocale = {locale};
          const selectLocale = () => {{
            const requested = requestedLocale.replaceAll('_', '-');
            const entries = Object.entries(baseConfig.locales ?? {{}});
            const exact = entries.find(([locale]) => locale.toLowerCase() === requested.toLowerCase());
            const language = requested.split('-')[0].toLowerCase();
            const fallback = entries.find(([locale]) => locale.toLowerCase() === language);
            return (exact ?? fallback)?.[1] ?? {{}};
          }};
          const syncLocaleConfig = () => {{
            const locale = selectLocale();
            const experience = locale.experience ?? {{}};
            const localizedHero = experience.homeHero ?? {{}};
            const localizedConversation = experience.conversationBanner ?? {{}};
            config = {{
              ...baseConfig,
              hero: {{
                ...baseConfig.hero,
                ...localizedHero,
                divider: baseConfig.hero.divider
                  ? {{ ...baseConfig.hero.divider, ...(localizedHero.divider ?? {{}}) }}
                  : null
              }},
              homePrompt: experience.homePrompt
                ? {{ ...(baseConfig.homePrompt ?? {{}}), ...experience.homePrompt }}
                : baseConfig.homePrompt,
              conversationBanner: baseConfig.conversationBanner
                ? {{ ...baseConfig.conversationBanner, ...localizedConversation }}
                : null
            }};
          }};
          syncLocaleConfig();
          const adapter = {adapter_config};
          const hasPro = {has_pro};
          const themeVersion = {theme_version};
          const runtimeKey = '__codexThemeRuntime';
          const sessionId = {session_id};
          const revokeAssetsUrl = {revoke_assets_url};
          const hardExpiresAt = {hard_expires_at};
          const pageLeaseMilliseconds = {page_lease_milliseconds};
          const restoreTheme = {restore_theme_function};
          const installPageLease = {lease_controller_function};
          const syncThemeStatus = () => {{
            const timer = document.querySelector(
              '[data-ct-slot="sidebar.footer.brand.timer"]'
            );
            if (!timer || !hardExpiresAt) return;
            const remaining = Math.max(0, Math.ceil((hardExpiresAt - Date.now()) / 1000));
            const minutes = Math.floor(remaining / 60);
            const seconds = remaining % 60;
            const value = `${{String(minutes).padStart(2, '0')}}:${{String(seconds).padStart(2, '0')}}`;
            if (timer.textContent !== value) timer.textContent = value;
          }};
          let assetsRevoked = false;
          const revokeAssets = () => {{
            if (assetsRevoked) return;
            assetsRevoked = true;
            const beacon = new Image();
            beacon.onload = beacon.onerror = () => beacon.remove();
            beacon.src = revokeAssetsUrl;
          }};
          const existingRuntime = window[runtimeKey];
          if (existingRuntime?.sessionId === sessionId && existingRuntime?.apply) {{
            const now = Date.now();
            if (existingRuntime.hardExpiresAt && now >= existingRuntime.hardExpiresAt) {{
              existingRuntime.restoreTheme?.(existingRuntime.sessionId, true);
              return [];
            }}
            existingRuntime.leaseExpiresAt = Math.min(
              now + pageLeaseMilliseconds,
              existingRuntime.hardExpiresAt ?? Number.POSITIVE_INFINITY
            );
            existingRuntime.syncThemeStatus?.();
            const refreshedSlots = existingRuntime.apply();
            if (refreshedSlots) existingRuntime.slots = refreshedSlots;
            return existingRuntime.slots ?? [];
          }}
          if (existingRuntime?.restoreTheme) {{
            existingRuntime.restoreTheme(existingRuntime.sessionId, false, true);
          }}
          else {{
            existingRuntime?.observer?.disconnect();
            existingRuntime?.schemeObserver?.disconnect();
            existingRuntime?.composerLayoutObserver?.disconnect();
            if (existingRuntime?.colorSchemeMedia && existingRuntime?.syncColorScheme) {{
              existingRuntime.colorSchemeMedia.removeEventListener('change', existingRuntime.syncColorScheme);
            }}
            const eventTarget = existingRuntime?.eventTarget ?? existingRuntime?.root;
            if (eventTarget && existingRuntime?.handleInput) {{
              eventTarget.removeEventListener('input', existingRuntime.handleInput, true);
            }}
            if (eventTarget && existingRuntime?.handleNavigation) {{
              eventTarget.removeEventListener('click', existingRuntime.handleNavigation, true);
            }}
            if (existingRuntime?.handleResize) {{
              window.removeEventListener('resize', existingRuntime.handleResize);
            }}
            if (existingRuntime?.frame) cancelAnimationFrame(existingRuntime.frame);
            if (existingRuntime?.homeFrame) cancelAnimationFrame(existingRuntime.homeFrame);
            if (existingRuntime?.contentFrame) cancelAnimationFrame(existingRuntime.contentFrame);
            if (existingRuntime?.metricsFrame) cancelAnimationFrame(existingRuntime.metricsFrame);
            if (existingRuntime?.resizeFrame) cancelAnimationFrame(existingRuntime.resizeFrame);
            if (existingRuntime?.leaseTimer) clearInterval(existingRuntime.leaseTimer);
            existingRuntime?.revokeAssets?.();
          }}
          document.querySelectorAll('[data-ct-managed-asset]').forEach(node => node.remove());

          const restoreSidebarFooterHeight = () => {{
            document.querySelectorAll('[data-ct-sidebar-footer-height]').forEach(node => {{
              const original = node.getAttribute('data-ct-sidebar-footer-height');
              if (original) node.style.setProperty('--sidebar-footer-height', original);
              else node.style.removeProperty('--sidebar-footer-height');
              node.removeAttribute('data-ct-sidebar-footer-height');
            }});
          }};
          restoreSidebarFooterHeight();

          const colorSchemeMedia = matchMedia('(prefers-color-scheme: light)');
          const syncColorScheme = () => {{
            const root = document.documentElement;
            if (!root) return 'dark';
            const scheme = root.classList.contains('electron-light')
              ? 'light'
              : root.classList.contains('electron-dark')
                ? 'dark'
                : colorSchemeMedia.matches ? 'light' : 'dark';
            root.setAttribute('data-ct-color-scheme', scheme);
            syncManagedAssetSchemes();
            return scheme;
          }};

          const createMount = (slot, className) => {{
            const mount = document.createElement('div');
            mount.dataset.ctMount = slot;
            mount.dataset.ctSlot = slot;
            mount.className = className;
            return mount;
          }};

          const createAssetMount = (slot, assetUrl) => {{
            const mount = document.createElement('span');
            mount.dataset.ctMount = slot;
            mount.dataset.ctSlot = slot;
            mount.dataset.ctManagedAsset = '';
            mount.setAttribute('aria-hidden', 'true');
            mount.style.pointerEvents = 'none';
            const image = document.createElement('img');
            image.alt = '';
            image.draggable = false;
            image.src = assetUrl;
            mount.appendChild(image);
            return mount;
          }};

          const assetsBySlot = new Map(
            (config.assets ?? []).map(asset => [asset.slot, asset])
          );

          const assetUrlForScheme = asset => {{
            if (!asset) return null;
            const scheme = document.documentElement?.dataset.ctColorScheme;
            return (scheme === 'light' ? asset.lightAssetUrl : asset.darkAssetUrl)
              ?? asset.assetUrl
              ?? null;
          }};

          const syncManagedAssetSchemes = () => {{
            document.querySelectorAll('[data-ct-managed-asset][data-ct-mount]')
              .forEach(mount => {{
                const asset = assetsBySlot.get(mount.getAttribute('data-ct-mount'));
                const assetUrl = assetUrlForScheme(asset);
                const image = mount.querySelector('img');
                if (image && assetUrl && image.src !== assetUrl) image.src = assetUrl;
              }});
          }};

          const syncAssetMount = (parent, slot, placement = 'append') => {{
            if (!parent) return null;
            const asset = assetsBySlot.get(slot);
            const assetUrl = assetUrlForScheme(asset);
            if (!assetUrl) return null;
            let mount = [...parent.children].find(
              child => child.getAttribute('data-ct-mount') === slot
            );
            if (!mount) {{
              mount = createAssetMount(slot, assetUrl);
              if (['app.background', 'main.background', 'main.overlay', 'main.frame', 'sidebar.frame'].includes(slot)) {{
                mount.style.position = 'absolute';
                mount.style.inset = '0';
              }}
              if (placement === 'prepend') parent.prepend(mount);
              else parent.appendChild(mount);
            }} else {{
              mount.dataset.ctSlot = slot;
              const image = mount.querySelector('img');
              if (image && image.src !== assetUrl) image.src = assetUrl;
            }}
            return mount;
          }};

          const syncCardArrowAsset = card => {{
            const asset = assetsBySlot.get('home.card.arrow.asset');
            const assetUrl = assetUrlForScheme(asset);
            if (!assetUrl) return null;
            let mount = [...card.children].find(
              child => child.getAttribute('data-ct-mount') === 'home.card.arrow.asset'
            );
            if (!mount) {{
              mount = document.createElement('span');
              mount.dataset.ctMount = 'home.card.arrow.asset';
              mount.dataset.ctSlot = 'home.card.arrow';
              mount.dataset.ctManagedAsset = '';
              mount.setAttribute('aria-hidden', 'true');
              mount.style.pointerEvents = 'none';
              const image = document.createElement('img');
              image.dataset.ctSlot = 'home.card.arrow.asset';
              image.alt = '';
              image.draggable = false;
              image.src = assetUrl;
              mount.appendChild(image);
              card.appendChild(mount);
            }}
            return mount;
          }};

          const setSlot = (node, slot) => {{
            if (node?.getAttribute('data-ct-slot') !== slot) {{
              node?.setAttribute('data-ct-slot', slot);
            }}
          }};

          const markSlot = (nodes, slot, slots) => {{
            const matched = [...nodes].filter(Boolean);
            matched.forEach(node => setSlot(node, slot));
            if (matched.length) slots.push(slot);
            return matched;
          }};

          const isCheckedControl = control =>
            control?.getAttribute('aria-checked') === 'true'
            || control?.getAttribute('data-state') === 'checked';

          const markMenus = slots => {{
            const menus = [...document.querySelectorAll('[role="menu"]')];
            markSlot(menus, 'menu', slots);
            menus.forEach(menu => {{
              const items = [...menu.querySelectorAll(
                '[role="menuitem"], [role="menuitemcheckbox"], [role="menuitemradio"]'
              )];
              markSlot(items, 'menu.item', slots);
              markSlot(
                items.filter(item => item.hasAttribute('data-highlighted')
                  || item.getAttribute('aria-current') === 'page'
                  || item.getAttribute('aria-current') === 'true'),
                'menu.item.active',
                slots
              );
              markSlot(
                items.filter(item => isCheckedControl(item)),
                'menu.item.checked',
                slots
              );
              markSlot(
                items.flatMap(item => [...item.querySelectorAll('svg')]),
                'menu.icon',
                slots
              );
              const shortcuts = items.flatMap(item => [...item.querySelectorAll('kbd')]);
              markSlot(shortcuts, 'menu.shortcut', slots);
              markSlot(
                items.flatMap(item => [...item.querySelectorAll('span, div, p')])
                  .filter(label => label.textContent?.trim()
                    && !label.querySelector('svg')
                    && !label.closest('kbd')
                    && !label.querySelector('kbd')),
                'menu.label',
                slots
              );
              markSlot(
                menu.querySelectorAll('[role="separator"], hr'),
                'menu.separator',
                slots
              );
            }});
          }};

          const markPage = (main, anchor, slots) => {{
            if (!main || !anchor || !main.contains(anchor)) return;
            let page = anchor;
            while (page.parentElement && page.parentElement !== main) {{
              page = page.parentElement;
            }}
            if (page === main) return;
            markSlot([page], 'page', slots);
            const pageRect = page.getBoundingClientRect();
            const surface = [...page.querySelectorAll('*')]
              .filter(node => {{
                if (!node.contains(anchor)) return false;
                const rect = node.getBoundingClientRect();
                const style = getComputedStyle(node);
                const hasSurface = style.backgroundColor !== 'rgba(0, 0, 0, 0)'
                  || style.backgroundImage !== 'none'
                  || style.boxShadow !== 'none'
                  || parseFloat(style.borderTopWidth) > 0;
                return hasSurface
                  && rect.width >= pageRect.width * 0.7
                  && rect.height >= pageRect.height * 0.7;
              }})
              .sort((left, right) =>
                right.getBoundingClientRect().width * right.getBoundingClientRect().height
                - left.getBoundingClientRect().width * left.getBoundingClientRect().height
              )[0];
            if (surface && surface !== page) markSlot([surface], 'page.surface', slots);
            const contentRoot = surface ?? page;
            const content = [...contentRoot.querySelectorAll('*')]
              .filter(node => node.contains(anchor))
              .find(node => ['auto', 'scroll'].includes(getComputedStyle(node).overflowY));
            if (content && content !== surface && content !== page) {{
              markSlot([content], 'page.content', slots);
            }}
            const contentRect = (content ?? surface ?? page).getBoundingClientRect();
            const header = [...page.children].find(node => {{
              if (node === content || node === surface || node.contains(anchor)) return false;
              const rect = node.getBoundingClientRect();
              return rect.height >= 28
                && rect.height <= 120
                && rect.width >= pageRect.width * 0.6
                && rect.bottom <= contentRect.top + 4;
            }});
            if (header) markSlot([header], 'page.header', slots);
          }};

          const markSettings = (settingsItems, settingsMain, slots) => {{
            if (!settingsItems.length) return;
            let settingsNav = settingsItems[0].closest('nav');
            if (!settingsNav) {{
              let candidate = settingsItems[0].parentElement;
              while (candidate && !settingsItems.every(item => candidate.contains(item))) {{
                candidate = candidate.parentElement;
              }}
              settingsNav = candidate;
            }}
            const settingsSidebar = settingsNav?.parentElement;
            const legacySettings = settingsSidebar?.parentElement;
            const legacyContent = legacySettings
              ? [...legacySettings.children].find(node => node !== settingsSidebar)
              : null;
            const settingsContent = legacyContent ?? settingsMain;
            let settings = settingsSidebar;
            while (settings && settingsContent && !settings.contains(settingsContent)) {{
              settings = settings.parentElement;
            }}
            const settingsNavRect = settingsNav?.getBoundingClientRect();
            const settingsSidebarRect = settingsSidebar?.getBoundingClientRect();
            const settingsHeader = settingsNavRect && settingsSidebarRect
              ? [...settingsSidebar.children].filter(node => {{
                  if (node === settingsNav) return false;
                  const rect = node.getBoundingClientRect();
                  return rect.height >= 28
                    && rect.width >= settingsSidebarRect.width * 0.8
                    && rect.top < settingsNavRect.top;
                }})
              : [];
            markSlot([settings], 'settings', slots);
            markSlot(settingsHeader, 'settings.header', slots);
            markSlot([settingsSidebar], 'settings.sidebar', slots);
            markSlot([settingsNav], 'settings.nav', slots);
            markSlot(settingsItems, 'settings.nav.item', slots);
            markSlot(
              settingsItems.filter(item => item.getAttribute('aria-current') === 'page'),
              'settings.nav.item.active',
              slots
            );
            markSlot([settingsContent], 'settings.content', slots);
            if (!settingsContent) return;

            const settingsFrame = settingsContent.querySelector(
              '[data-ct-slot="main.content.frame"]'
            ) ?? (adapter.selectors.mainContentFrame
              ? settingsContent.querySelector(adapter.selectors.mainContentFrame)
              : null);
            markSlot([settingsFrame], 'settings.frame', slots);

            const surface = [...settingsContent.children]
              .filter(node => isVisible(node))
              .sort((left, right) =>
                right.getBoundingClientRect().width * right.getBoundingClientRect().height
                - left.getBoundingClientRect().width * left.getBoundingClientRect().height
              )[0];
            markSlot([surface], 'settings.surface', slots);
            const body = [surface, ...(surface?.querySelectorAll('*') ?? [])]
              .filter(Boolean)
              .filter(node => ['auto', 'scroll'].includes(getComputedStyle(node).overflowY))
              .sort((left, right) =>
                right.getBoundingClientRect().width * right.getBoundingClientRect().height
                - left.getBoundingClientRect().width * left.getBoundingClientRect().height
              )[0];
            markSlot([body], 'settings.body', slots);
            const canvas = body?.parentElement;
            if (canvas && canvas !== surface && canvas !== settingsContent) {{
              markSlot([canvas], 'settings.canvas', slots);
              const bodyRect = body.getBoundingClientRect();
              const canvasRect = canvas.getBoundingClientRect();
              const toolbar = [...canvas.children].find(node => {{
                if (node === body || !isVisible(node)) return false;
                const rect = node.getBoundingClientRect();
                return rect.height >= 28
                  && rect.height <= 80
                  && rect.width >= canvasRect.width * 0.7
                  && rect.top <= bodyRect.top;
              }});
              markSlot([toolbar], 'settings.toolbar', slots);
            }}

            const settingsControls = [...settingsContent.querySelectorAll(
              'input, textarea, select, [role="switch"], [role="checkbox"], '
                + '[role="radio"], [role="slider"], [role="combobox"]'
            )];
            const rowAnchors = [...new Set([
              ...settingsControls,
              ...settingsContent.querySelectorAll('button, [role="button"]')
            ])];
            const contentRect = (body ?? surface ?? settingsContent).getBoundingClientRect();
            const rows = [...new Set(rowAnchors.map(anchor => {{
              let candidate = anchor.parentElement;
              while (candidate && candidate !== settingsContent) {{
                const rect = candidate.getBoundingClientRect();
                if (rect.width >= contentRect.width * 0.45
                  && rect.height >= 32
                  && rect.height <= 180
                  && candidate.textContent?.trim()) {{
                  return candidate;
                }}
                candidate = candidate.parentElement;
              }}
              return null;
            }}).filter(Boolean))];
            const cards = [];
            const addCard = candidate => {{
              if (cards.some(card => candidate.contains(card))) return;
              cards.filter(card => card.contains(candidate))
                .forEach(card => cards.splice(cards.indexOf(card), 1));
              cards.push(candidate);
            }};
            rows.forEach(row => {{
              let candidate = row.parentElement;
              while (candidate && candidate !== settingsContent) {{
                const style = getComputedStyle(candidate);
                const parentStyle = candidate.parentElement
                  ? getComputedStyle(candidate.parentElement)
                  : null;
                const hasBoundary = parseFloat(style.borderTopLeftRadius) >= 8
                  || parseFloat(style.borderTopWidth) > 0
                  || style.boxShadow !== 'none'
                  || style.backgroundImage !== 'none'
                  || (style.backgroundColor !== 'rgba(0, 0, 0, 0)'
                    && style.backgroundColor !== parentStyle?.backgroundColor);
                if (hasBoundary) {{
                  addCard(candidate);
                  break;
                }}
                const rowGroups = [...candidate.children].filter(child =>
                  rows.some(item => child === item || child.contains(item))
                );
                if (rowGroups.length >= 2) {{
                  addCard(candidate);
                  break;
                }}
                candidate = candidate.parentElement;
              }}
            }});
            if (!cards.length) {{
              [...settingsContent.querySelectorAll('div')].forEach(candidate => {{
                const rect = candidate.getBoundingClientRect();
                const style = getComputedStyle(candidate);
                if (candidate.children.length > 0
                  && candidate.querySelector('input, select, [role="switch"], [role="checkbox"]')
                  && rect.width >= contentRect.width * 0.45
                  && rect.height >= 32
                  && parseFloat(style.borderTopLeftRadius) >= 8
                  && !cards.some(card => candidate.contains(card))) {{
                  addCard(candidate);
                }}
              }});
            }}
            const sections = [...new Set([
              ...(body?.querySelectorAll('section') ?? []),
              ...cards.map(card => card.parentElement)
                .filter(section => section && section !== body && section !== surface)
            ])];
            markSlot(sections, 'settings.section', slots);
            sections.forEach(section => {{
              let title = section.firstElementChild;
              while (title) {{
                const textChildren = [...title.children]
                  .filter(node => node.textContent?.trim());
                if (textChildren.length !== 1) break;
                title = textChildren[0];
              }}
              if (title?.textContent?.trim()) markSlot([title], 'settings.section.title', slots);
            }});
            markSlot(cards, 'settings.card', slots);
            cards.forEach(card => {{
              const cardRows = rows.filter(row => card.contains(row));
              markSlot(cardRows, 'settings.row', slots);
              markSlot(cardRows.slice(0, -1), 'settings.row.separator', slots);
              cardRows.forEach(row => {{
                const control = row.querySelector(
                  'input, textarea, select, [role="switch"], [role="checkbox"], '
                    + '[role="radio"], [role="slider"], [role="combobox"]'
                );
                let copy = [...row.children].find(node =>
                  !node.contains(control) && node.textContent?.trim()
                );
                while (copy) {{
                  const textChildren = [...copy.children]
                    .filter(node => node.textContent?.trim() && !node.contains(control));
                  if (textChildren.length !== 1) break;
                  copy = textChildren[0];
                }}
                const copyParts = [...(copy?.children ?? [])]
                  .filter(node => node.textContent?.trim());
                if (copyParts.length) {{
                  markSlot([copyParts[0]], 'settings.row.title', slots);
                  markSlot(copyParts.slice(1), 'settings.row.description', slots);
                }} else if (copy?.textContent?.trim()) {{
                  markSlot([copy], 'settings.row.title', slots);
                }}
              }});
            }});

            const switches = settingsControls.filter(control => control.matches('[role="switch"]'));
            markSlot(
              settingsControls.filter(control => !isCheckedControl(control)),
              'settings.control',
              slots
            );
            markSlot(
              settingsControls.filter(control => isCheckedControl(control)),
              'settings.control.checked',
              slots
            );
            const switchContainers = switches.map(control => control.parentElement);
            markSlot(
              switchContainers.filter((_, index) => !isCheckedControl(switches[index])),
              'settings.switch',
              slots
            );
            markSlot(
              switchContainers.filter((_, index) => isCheckedControl(switches[index])),
              'settings.switch.checked',
              slots
            );
            const uncheckedTracks = switches
              .filter(control => !isCheckedControl(control))
              .map(control => control.firstElementChild);
            const checkedTracks = switches
              .filter(control => isCheckedControl(control))
              .map(control => control.firstElementChild);
            markSlot(uncheckedTracks, 'settings.switch.track', slots);
            markSlot(checkedTracks, 'settings.switch.track.checked', slots);
            markSlot(
              switches.map(control => control.firstElementChild?.firstElementChild),
              'settings.switch.thumb',
              slots
            );
          }};

          const isVisible = node => {{
            if (!node?.isConnected) return false;
            const rect = node.getBoundingClientRect();
            const style = getComputedStyle(node);
            return rect.width > 0
              && rect.height > 0
              && style.display !== 'none'
              && style.visibility !== 'hidden';
          }};

          const setStyleProperty = (node, property, value) => {{
            if (!node || node.style.getPropertyValue(property) === value) return;
            node.style.setProperty(property, value);
          }};

          const syncTitlebarSafeTop = appMain => {{
            if (!appMain) return;
            const mainTop = appMain.getBoundingClientRect().top;
            const titlebarBottom = [...document.querySelectorAll(adapter.selectors.titlebar)]
              .filter(isVisible)
              .reduce(
                (bottom, titlebar) => Math.max(bottom, titlebar.getBoundingClientRect().bottom),
                mainTop
              );
            setStyleProperty(
              document.documentElement,
              '--ct-titlebar-safe-top',
              `${{Math.round(Math.max(0, titlebarBottom - mainTop))}}px`
            );
          }};

          const clearConversationHeaderMetrics = stage => {{
            if (!stage) return;
            stage.style.removeProperty('--ct-conversation-content-left');
            stage.style.removeProperty('--ct-conversation-content-width');
            stage.style.removeProperty('--ct-conversation-header-safe-top');
          }};

          const syncConversationHeaderLayout = () => {{
            const stage = document.querySelector('[data-ct-slot="conversation.stage"]');
            const header = stage?.querySelector(
              ':scope > [data-ct-slot="conversation.header"]'
            );
            const composerRoot = [...document.querySelectorAll(
              adapter.selectors.composerRoot
            )].find(isVisible);
            if (!stage || !header || !composerRoot) {{
              clearConversationHeaderMetrics(stage);
              return;
            }}
            const stageRect = stage.getBoundingClientRect();
            const composerRect = composerRoot.getBoundingClientRect();
            if (stageRect.width <= 0 || composerRect.width <= 0) {{
              clearConversationHeaderMetrics(stage);
              return;
            }}
            const contentLeft = Math.max(
              0,
              Math.min(stageRect.width, composerRect.left - stageRect.left)
            );
            const contentRight = Math.max(
              0,
              Math.min(stageRect.width - contentLeft, stageRect.right - composerRect.right)
            );
            const contentWidth = Math.max(0, stageRect.width - contentLeft - contentRight);
            const titlebarBottom = [...document.querySelectorAll(adapter.selectors.titlebar)]
              .filter(isVisible)
              .reduce(
                (bottom, titlebar) => Math.max(bottom, titlebar.getBoundingClientRect().bottom),
                stageRect.top
              );
            setStyleProperty(
              stage,
              '--ct-conversation-content-left',
              `${{contentLeft.toFixed(2)}}px`
            );
            setStyleProperty(
              stage,
              '--ct-conversation-content-width',
              `${{contentWidth.toFixed(2)}}px`
            );
            setStyleProperty(
              stage,
              '--ct-conversation-header-safe-top',
              `${{Math.max(0, titlebarBottom - stageRect.top).toFixed(2)}}px`
            );
          }};

          const editorHasDraft = editor => {{
            if (!editor) return false;
            const input = editor.matches('textarea, input, [contenteditable="true"]')
              ? editor
              : editor.querySelector('textarea, input, [contenteditable="true"]');
            const value = input && 'value' in input ? input.value : input?.textContent;
            return Boolean(value?.trim());
          }};

          const syncComposerLayout = editor => {{
            if (!editor?.isConnected) return;
            const frameHeight = editor.parentElement?.getBoundingClientRect().height ?? 0;
            const layout = frameHeight > 0 && frameHeight <= 44
              ? 'compact'
              : 'expanded';
            if (editor.getAttribute('data-ct-composer-layout') !== layout) {{
              editor.setAttribute('data-ct-composer-layout', layout);
            }}
          }};

          let composerLayoutTarget = null;
          const composerLayoutObserver = new ResizeObserver(() => {{
            syncComposerLayout(
              document.querySelector('[data-ct-slot="composer.editor"]')
            );
          }});
          const observeComposerLayout = editor => {{
            const target = editor?.parentElement ?? null;
            if (composerLayoutTarget === target) return;
            composerLayoutObserver.disconnect();
            composerLayoutTarget = target;
            if (target) composerLayoutObserver.observe(target);
          }};

          const restoreHomePrompt = () => {{
            document.querySelectorAll('[data-ct-home-prompt-native]').forEach(node => {{
              const display = node.getAttribute('data-ct-home-prompt-display');
              const priority = node.getAttribute('data-ct-home-prompt-display-priority') ?? '';
              if (display === null) node.style.removeProperty('display');
              else node.style.setProperty('display', display, priority);
              node.removeAttribute('data-ct-home-prompt-native');
              node.removeAttribute('data-ct-home-prompt-display');
              node.removeAttribute('data-ct-home-prompt-display-priority');
            }});
            document.querySelectorAll('[data-ct-mount="home.prompt.title"]')
              .forEach(node => node.remove());
          }};

          const clearHomeSlotMarkers = () => {{
            document.querySelectorAll('[data-ct-slot^="home."]').forEach(node => {{
              if (!node.closest('[data-ct-mount]')) node.removeAttribute('data-ct-slot');
            }});
          }};

          const clearHomeSlots = main => {{
            restoreHomePrompt();
            main.querySelectorAll(
              '[data-ct-mount="home.hero"], [data-ct-mount="home.card.background"]'
            ).forEach(node => node.remove());
            clearHomeSlotMarkers();
            document.documentElement.style.removeProperty('--ct-home-card-count');
          }};

          const clearConversationLayout = main => {{
            main.querySelectorAll(
              '[data-ct-mount="conversation.header"], '
                + '[data-ct-mount="conversation.banner"], '
                + '[data-ct-mount="conversation.summary.decoration"]'
            )
              .forEach(node => node.remove());
            main.querySelectorAll(
              '[data-ct-slot="conversation.header"], '
                + '[data-ct-slot="conversation.header.content"], '
                + '[data-ct-slot="conversation.stage"], '
                + '[data-ct-slot="conversation.viewport"], '
                + '[data-ct-slot="conversation.summary.region"], '
                + '[data-ct-slot="conversation.summary"]'
            ).forEach(node => {{
              node.style.removeProperty('--ct-conversation-banner-clearance');
              node.style.removeProperty('--ct-conversation-summary-width');
              node.style.removeProperty('--ct-conversation-content-left');
              node.style.removeProperty('--ct-conversation-content-width');
              node.style.removeProperty('--ct-conversation-header-safe-top');
              node.removeAttribute('data-ct-slot');
            }});
          }};

          const markConversationSummary = (stage, slots) => {{
            const summarySelector = adapter.selectors.conversationSummaryRegion;
            const summary = summarySelector
              ? [...document.querySelectorAll(summarySelector)].find(isVisible)
              : null;
            const clearSummary = () => {{
              document.querySelectorAll(
                '[data-ct-slot="conversation.summary.region"], '
                  + '[data-ct-slot="conversation.summary"]'
              ).forEach(node => node.removeAttribute('data-ct-slot'));
              document.querySelectorAll(
                '[data-ct-mount="conversation.summary.decoration"]'
              ).forEach(node => node.remove());
              if (stage.style.getPropertyValue('--ct-conversation-summary-width')) {{
                stage.style.removeProperty('--ct-conversation-summary-width');
              }}
            }};
            if (!summary) {{
              clearSummary();
              return;
            }}
            let summaryRegion = summary;
            while (
              summaryRegion.parentElement
              && summaryRegion.parentElement !== stage
              && getComputedStyle(summaryRegion).position !== 'absolute'
            ) {{
              summaryRegion = summaryRegion.parentElement;
            }}
            if (getComputedStyle(summaryRegion).position !== 'absolute') {{
              clearSummary();
              return;
            }}
            document.querySelectorAll('[data-ct-slot="conversation.summary.region"]')
              .forEach(node => {{
                if (node !== summaryRegion) node.removeAttribute('data-ct-slot');
              }});
            setSlot(summaryRegion, 'conversation.summary.region');
            const summarySurface = [...summary.children].find(
              node => !node.matches('[data-ct-mount]')
            ) ?? summary;
            document.querySelectorAll('[data-ct-slot="conversation.summary"]')
              .forEach(node => {{
                if (node !== summarySurface) node.removeAttribute('data-ct-slot');
              }});
            setSlot(summarySurface, 'conversation.summary');
            slots.push('conversation.summary.region', 'conversation.summary');
            const summaryDecorationUrl = config.conversationSummaryDecoration?.assetUrl;
            document.querySelectorAll(
              '[data-ct-mount="conversation.summary.decoration"]'
            ).forEach(node => {{
              if (!summaryDecorationUrl || node.parentElement !== summarySurface) node.remove();
            }});
            if (summaryDecorationUrl) {{
              let decoration = summarySurface.querySelector(
                ':scope > [data-ct-mount="conversation.summary.decoration"]'
              );
              if (!decoration) {{
                decoration = createAssetMount(
                  'conversation.summary.decoration',
                  summaryDecorationUrl
                );
                summarySurface.appendChild(decoration);
              }}
              slots.push('conversation.summary.decoration');
            }}
            const stageRect = stage.getBoundingClientRect();
            const summaryRect = summaryRegion.getBoundingClientRect();
            const summaryWidth = Math.max(
              0,
              Math.min(stageRect.width, stageRect.right - summaryRect.left)
            );
            setStyleProperty(
              stage,
              '--ct-conversation-summary-width',
              `${{Math.round(summaryWidth)}}px`
            );
          }};

          const syncConversationBannerForeground = () => {{
            const foreground = document.querySelector(
              '[data-ct-slot="conversation.banner.foreground"]'
            );
            const banner = foreground?.closest('[data-ct-slot="conversation.banner"]');
            if (!foreground || !banner || !isVisible(banner)) return;
            let clipTop = window.visualViewport?.offsetTop ?? 0;
            for (
              let ancestor = banner.parentElement;
              ancestor;
              ancestor = ancestor.parentElement
            ) {{
              if (!['auto', 'scroll', 'hidden', 'clip'].includes(
                getComputedStyle(ancestor).overflowY
              )) continue;
              clipTop = Math.max(
                clipTop,
                ancestor.getBoundingClientRect().top + ancestor.clientTop
              );
            }}
            const foregroundBottom = foreground.getBoundingClientRect().bottom;
            setStyleProperty(
              foreground,
              '--ct-conversation-banner-foreground-safe-height',
              `${{Math.max(0, Math.floor(foregroundBottom - clipTop))}}px`
            );
          }};

          const mountConversationBanner = (main, editor, slots, target) => {{
            const currentBanner = main.querySelector(
              '[data-ct-mount="conversation.banner"]'
            );
            if (!isVisible(editor) || !isVisible(target)) {{
              clearConversationLayout(main);
              return;
            }}
            const isConversationTarget = target.matches(adapter.selectors.conversation);
            const viewport = isConversationTarget ? target.parentElement : null;
            const stage = viewport?.parentElement;
            if (isConversationTarget) {{
              if (!stage || stage === main || viewport.parentElement !== stage) {{
                clearConversationLayout(main);
                return;
              }}
              main.querySelectorAll('[data-ct-slot="conversation.stage"]')
                .forEach(node => {{
                  if (node === stage) return;
                  node.querySelector(':scope > [data-ct-mount="conversation.header"]')?.remove();
                  node.style.removeProperty('--ct-conversation-summary-width');
                  clearConversationHeaderMetrics(node);
                  node.removeAttribute('data-ct-slot');
                }});
              main.querySelectorAll('[data-ct-slot="conversation.viewport"]')
                .forEach(node => {{
                  if (node !== viewport) node.removeAttribute('data-ct-slot');
                }});
              setSlot(stage, 'conversation.stage');
              setSlot(viewport, 'conversation.viewport');
              slots.push('conversation.stage', 'conversation.viewport');
            }}
            const bannerConfig = config.conversationBanner;
            if (!bannerConfig) {{
              currentBanner?.remove();
              main.querySelector('[data-ct-mount="conversation.header"]')?.remove();
              return;
            }}
            let banner = currentBanner;
            if (!banner) {{
              banner = createMount('conversation.banner', 'ct-conversation-banner');
              const copy = document.createElement('div');
              copy.dataset.ctSlot = 'conversation.banner.copy';
              const eyebrow = document.createElement('p');
              eyebrow.dataset.ctSlot = 'conversation.banner.eyebrow';
              eyebrow.textContent = bannerConfig.eyebrow;
              const title = document.createElement('h2');
              title.dataset.ctSlot = 'conversation.banner.title';
              title.textContent = bannerConfig.title;
              const description = document.createElement('p');
              description.dataset.ctSlot = 'conversation.banner.description';
              description.textContent = bannerConfig.description;
              copy.append(eyebrow, title, description);
              const media = document.createElement('div');
              media.dataset.ctSlot = 'conversation.banner.media';
              const image = document.createElement('img');
              image.dataset.ctSlot = 'conversation.banner.media.asset';
              image.alt = '';
              image.draggable = false;
              image.src = bannerConfig.assetUrl;
              image.style.objectFit = bannerConfig.fit;
              image.style.objectPosition = bannerConfig.position;
              media.appendChild(image);
              const foreground = bannerConfig.foregroundAssetUrl
                ? createAssetMount(
                    'conversation.banner.foreground',
                    bannerConfig.foregroundAssetUrl
                  )
                : null;
              if (foreground) {{
                foreground.querySelector('img').dataset.ctSlot =
                  'conversation.banner.foreground.asset';
              }}
              banner.append(copy, media);
              if (foreground) banner.appendChild(foreground);
            }}
            const bannerEyebrow = banner.querySelector(
              '[data-ct-slot="conversation.banner.eyebrow"]'
            );
            const bannerTitle = banner.querySelector(
              '[data-ct-slot="conversation.banner.title"]'
            );
            const bannerDescription = banner.querySelector(
              '[data-ct-slot="conversation.banner.description"]'
            );
            if (bannerEyebrow.textContent !== bannerConfig.eyebrow) {{
              bannerEyebrow.textContent = bannerConfig.eyebrow;
            }}
            if (bannerTitle.textContent !== bannerConfig.title) {{
              bannerTitle.textContent = bannerConfig.title;
            }}
            if (bannerDescription.textContent !== bannerConfig.description) {{
              bannerDescription.textContent = bannerConfig.description;
            }}
            if (isConversationTarget) {{
              let header = stage.querySelector(
                ':scope > [data-ct-mount="conversation.header"]'
              );
              if (!header) {{
                header = createMount('conversation.header', 'ct-conversation-header');
                const content = document.createElement('div');
                content.dataset.ctSlot = 'conversation.header.content';
                header.appendChild(content);
              }}
              setSlot(header, 'conversation.header');
              const content = header.firstElementChild;
              setSlot(content, 'conversation.header.content');
              if (header.parentElement !== stage || header.nextElementSibling !== viewport) {{
                stage.insertBefore(header, viewport);
              }}
              if (banner.parentElement !== content) {{
                content.appendChild(banner);
              }}
              syncConversationHeaderLayout();
            }} else if (banner.parentElement !== target || target.firstElementChild !== banner) {{
              target.prepend(banner);
            }}
            slots.push(
              ...(isConversationTarget
                ? ['conversation.header', 'conversation.header.content']
                : []),
              'conversation.banner',
              'conversation.banner.copy',
              'conversation.banner.eyebrow',
              'conversation.banner.title',
              'conversation.banner.description',
              'conversation.banner.media',
              'conversation.banner.media.asset'
            );
            if (banner.querySelector('[data-ct-slot="conversation.banner.foreground"]')) {{
              slots.push(
                'conversation.banner.foreground',
                'conversation.banner.foreground.asset'
              );
              syncConversationBannerForeground();
            }}
            if (isConversationTarget) markConversationSummary(stage, slots);
          }};

          const mountHero = (main, editor, slots) => {{
            const homeSource = main.querySelector(adapter.selectors.homeSource);
            const composerRoot = editor?.closest(adapter.selectors.composerRoot) ?? editor;
            let homeLayout = homeSource;
            while (
              homeLayout
              && homeLayout !== main
              && composerRoot
              && !homeLayout.contains(composerRoot)
            ) {{
              homeLayout = homeLayout.parentElement;
            }}
            if (!composerRoot || homeLayout === main) homeLayout = null;
            const directLayoutBranch = descendant => {{
              if (!homeLayout?.contains(descendant)) return null;
              let branch = descendant;
              while (branch?.parentElement && branch.parentElement !== homeLayout) {{
                branch = branch.parentElement;
              }}
              return branch?.parentElement === homeLayout ? branch : null;
            }};
            const homeBranch = directLayoutBranch(homeSource);
            let homeStage = homeSource;
            while (
              homeStage
              && homeStage !== homeBranch
              && !homeStage.querySelector(adapter.selectors.homeCards)
            ) {{
              homeStage = homeStage.parentElement;
            }}
            const composerRegion = directLayoutBranch(composerRoot);
            const originalHomeContent = homeStage?.querySelector(adapter.selectors.homeCards);
            const homeCards = [...(originalHomeContent?.querySelectorAll('button') ?? [])];
            const conversation = [...document.querySelectorAll(
              adapter.selectors.conversation
            )].find(isVisible);
            const isConversation = Boolean(conversation);
            const hasHomePrompt = isVisible(editor)
              && !isConversation
              && Boolean(homeSource?.isConnected && homeLayout && homeBranch && composerRegion);
            const hasWorkspacePanel = Boolean(adapter.selectors.workspacePanel)
              && [...document.querySelectorAll(adapter.selectors.workspacePanel)]
                .some(isVisible);
            const isCompactHome = hasHomePrompt
              && (editorHasDraft(editor) || hasWorkspacePanel);
            const isHome = hasHomePrompt
              && (isCompactHome
                || (Boolean(originalHomeContent?.isConnected)
                  && homeCards.some(card => card.isConnected)));
            if (!isHome) {{
              document.documentElement.setAttribute(
                'data-ct-view',
                isConversation ? 'conversation' : 'other'
              );
              clearHomeSlots(main);
              main.querySelectorAll('[data-ct-mount^="decoration."]')
                .forEach(node => node.remove());
              mountConversationBanner(
                main,
                editor,
                slots,
                isConversation ? conversation : null
              );
              return;
            }}
            document.documentElement.setAttribute(
              'data-ct-view',
              isCompactHome ? 'home-compact' : 'home'
            );
            clearConversationLayout(main);
            clearHomeSlotMarkers();
            if (homeLayout) {{
              homeLayout.setAttribute('data-ct-slot', 'home.layout');
              slots.push('home.layout');
            }}
            if (homeBranch) {{
              homeBranch.setAttribute('data-ct-slot', 'home.content.region');
              slots.push('home.content.region');
            }}
            if (homeStage) {{
              homeStage.setAttribute('data-ct-slot', 'home.stage');
              slots.push('home.stage');
            }}
            if (composerRegion) {{
              composerRegion.setAttribute('data-ct-slot', 'composer.region');
              slots.push('composer.region');
            }}
            const homeBrand = adapter.selectors.homeBrand
              ? homeStage?.querySelector(adapter.selectors.homeBrand)
              : null;
            if (homeBrand) {{
              homeBrand.setAttribute('data-ct-slot', 'home.brand');
              slots.push('home.brand');
            }}
            if (homeSource) {{
              homeSource.setAttribute('data-ct-slot', 'home.prompt');
              slots.push('home.prompt');
              const promptConfig = config.homePrompt;
              let promptTitle = homeSource.querySelector(
                ':scope > [data-ct-mount="home.prompt.title"]'
              );
              const nativePromptNodes = [...homeSource.children]
                .filter(node => node !== promptTitle);
              if (promptConfig?.title) {{
                nativePromptNodes.forEach(node => {{
                  if (!node.hasAttribute('data-ct-home-prompt-native')) {{
                    const display = node.style.getPropertyValue('display');
                    if (display) node.setAttribute('data-ct-home-prompt-display', display);
                    const priority = node.style.getPropertyPriority('display');
                    if (priority) {{
                      node.setAttribute('data-ct-home-prompt-display-priority', priority);
                    }}
                    node.setAttribute('data-ct-home-prompt-native', '');
                  }}
                  node.removeAttribute('data-ct-slot');
                  node.style.setProperty('display', 'none', 'important');
                }});
                if (!promptTitle) {{
                  promptTitle = createMount('home.prompt.title', 'ct-home-prompt-title');
                  homeSource.appendChild(promptTitle);
                }}
                if (promptTitle.textContent !== promptConfig.title) {{
                  promptTitle.textContent = promptConfig.title;
                }}
                slots.push('home.prompt.title');
              }} else {{
                restoreHomePrompt();
                const nativeTitle = homeSource.firstElementChild;
                if (nativeTitle) {{
                  nativeTitle.setAttribute('data-ct-slot', 'home.prompt.title');
                  slots.push('home.prompt.title');
                }}
              }}
            }}
            if (originalHomeContent) {{
              originalHomeContent.setAttribute('data-ct-slot', 'home.cards');
              slots.push('home.cards');
              const cards = homeCards;
              document.documentElement.style.setProperty(
                '--ct-home-card-count',
                String(cards.length)
              );
              const cardsLayout = [];
              for (
                let node = originalHomeContent.parentElement;
                node && node !== homeStage;
                node = node.parentElement
              ) {{
                cardsLayout.push(node);
              }}
              markSlot(cardsLayout, 'home.cards.layout', slots);
              const cardsGrid = cards[0]?.parentElement?.parentElement;
              if (cardsGrid && cards.every(card => cardsGrid.contains(card))) {{
                cardsGrid.setAttribute('data-ct-slot', 'home.cards.grid');
                slots.push('home.cards.grid');
              }}
              cards.forEach(card => {{
                card.setAttribute('data-ct-slot', 'home.card');
                let background = [...card.children].find(node =>
                  node.getAttribute('data-ct-mount') === 'home.card.background'
                );
                if (!background) {{
                  background = createMount('home.card.background', 'ct-home-card-background');
                  background.setAttribute('aria-hidden', 'true');
                  background.style.pointerEvents = 'none';
                  card.prepend(background);
                }}
                const backgroundAsset = assetsBySlot.get('home.card.background');
                const backgroundAssetUrl = assetUrlForScheme(backgroundAsset);
                if (backgroundAssetUrl) {{
                  let backgroundImage = background.querySelector('img');
                  if (!backgroundImage) {{
                    backgroundImage = document.createElement('img');
                    backgroundImage.alt = '';
                    backgroundImage.draggable = false;
                    background.appendChild(backgroundImage);
                  }}
                  if (backgroundImage.src !== backgroundAssetUrl) {{
                    backgroundImage.src = backgroundAssetUrl;
                  }}
                }}
                const iconContainers = [...card.children].filter(node => node.querySelector('svg'));
                const icon = iconContainers[0];
                const nativeArrow = iconContainers.length > 1
                  ? iconContainers[iconContainers.length - 1]
                  : null;
                const labelContainer = [...card.children].find(
                  node => node !== icon && node !== nativeArrow && node.textContent?.trim()
                );
                let label = labelContainer;
                while (label) {{
                  const textChildren = [...label.children].filter(node =>
                    node.textContent?.trim()
                    && !node.querySelector('svg')
                    && !node.matches('[data-ct-mount]')
                  );
                  if (textChildren.length !== 1) break;
                  label = textChildren[0];
                }}
                const content = labelContainer !== label
                  ? labelContainer
                  : label?.parentElement !== card ? label?.parentElement : null;
                if (content) content.setAttribute('data-ct-slot', 'home.card.content');
                if (icon) {{
                  icon.setAttribute('data-ct-slot', 'home.card.icon');
                  icon.querySelector('svg')?.setAttribute('data-ct-slot', 'home.card.icon.glyph');
                }}
                if (label) label.setAttribute('data-ct-slot', 'home.card.label');
                if (nativeArrow) {{
                  nativeArrow.setAttribute('data-ct-slot', 'home.card.arrow');
                  nativeArrow.querySelector('svg')?.setAttribute('data-ct-slot', 'home.card.arrow.glyph');
                }}
                syncCardArrowAsset(card);
              }});
              if (cards.length) slots.push('home.card');
              if (cards.some(card => card.querySelector(':scope > [data-ct-slot="home.card.background"]'))) {{
                slots.push('home.card.background');
              }}
              if (cards.some(card => card.querySelector('[data-ct-slot="home.card.content"]'))) {{
                slots.push('home.card.content');
              }}
              if (cards.some(card => card.querySelector('[data-ct-slot="home.card.icon"]'))) {{
                slots.push('home.card.icon');
              }}
              if (cards.some(card => card.querySelector('[data-ct-slot="home.card.icon.glyph"]'))) {{
                slots.push('home.card.icon.glyph');
              }}
              if (cards.some(card => card.querySelector('[data-ct-slot="home.card.label"]'))) {{
                slots.push('home.card.label');
              }}
              if (cards.some(card => card.querySelector('[data-ct-slot="home.card.arrow"]'))) {{
                slots.push('home.card.arrow');
              }}
              if (cards.some(card => card.querySelector('[data-ct-slot="home.card.arrow.glyph"]'))) {{
                slots.push('home.card.arrow.glyph');
              }}
              if (cards.some(card => card.querySelector('[data-ct-slot="home.card.arrow.asset"]'))) {{
                slots.push('home.card.arrow.asset');
              }}
            }}

            if (isCompactHome) {{
              main.querySelectorAll(
                '[data-ct-mount="home.hero"], [data-ct-mount^="decoration."]'
              ).forEach(node => node.remove());
              if (!hasWorkspacePanel) {{
                mountConversationBanner(main, editor, slots, homeLayout);
              }}
              return;
            }}

            let hero = main.querySelector('[data-ct-mount="home.hero"]');
            if (!hero) {{
              hero = createMount('home.hero', 'ct-home-hero');
              const viewport = document.createElement('div');
              viewport.dataset.ctSlot = 'home.hero.viewport';
              const copy = document.createElement('div');
              copy.className = 'ct-home-hero__copy';
              copy.dataset.ctSlot = 'home.hero.copy';
              const eyebrow = document.createElement('p');
              eyebrow.className = 'ct-home-hero__eyebrow';
              eyebrow.dataset.ctSlot = 'home.hero.eyebrow';
              eyebrow.textContent = config.hero.eyebrow;
              const title = document.createElement('h1');
              title.className = 'ct-home-hero__title';
              title.dataset.ctSlot = 'home.hero.title';
              title.textContent = config.hero.title;
              const description = document.createElement('p');
              description.className = 'ct-home-hero__description';
              description.dataset.ctSlot = 'home.hero.description';
              description.textContent = config.hero.description;
              copy.append(eyebrow, title, description);
              const media = document.createElement('div');
              media.dataset.ctSlot = 'home.hero.media';
              const image = document.createElement('img');
              image.className = 'ct-home-hero__image';
              image.dataset.ctSlot = 'home.hero.media.asset';
              image.alt = '';
              image.draggable = false;
              image.src = config.hero.assetUrl;
              image.style.objectFit = config.hero.fit;
              image.style.objectPosition = config.hero.position;
              media.appendChild(image);
              const foreground = config.hero.foregroundAssetUrl
                ? createAssetMount('home.hero.foreground', config.hero.foregroundAssetUrl)
                : null;
              if (foreground) {{
                foreground.querySelector('img').dataset.ctSlot = 'home.hero.foreground.asset';
              }}
              const divider = document.createElement('div');
              divider.dataset.ctSlot = 'home.hero.divider';
              divider.setAttribute('aria-hidden', 'true');
              const dividerIcon = document.createElement('span');
              dividerIcon.dataset.ctSlot = 'home.hero.divider.icon';
              if (config.hero.divider?.assetUrl) {{
                const dividerImage = document.createElement('img');
                dividerImage.alt = '';
                dividerImage.draggable = false;
                dividerImage.src = config.hero.divider.assetUrl;
                dividerIcon.appendChild(dividerImage);
              }}
              const dividerLabel = document.createElement('span');
              dividerLabel.dataset.ctSlot = 'home.hero.divider.label';
              dividerLabel.textContent = config.hero.divider?.label ?? '';
              const dividerLine = document.createElement('span');
              dividerLine.dataset.ctSlot = 'home.hero.divider.line';
              divider.append(dividerIcon, dividerLabel, dividerLine);
              viewport.append(copy, media, divider);
              hero.appendChild(viewport);
              if (foreground) hero.appendChild(foreground);
              homeStage.prepend(hero);
            }}
            let heroViewport = hero.querySelector(
              ':scope > [data-ct-slot="home.hero.viewport"]'
            );
            if (!heroViewport) {{
              heroViewport = document.createElement('div');
              heroViewport.dataset.ctSlot = 'home.hero.viewport';
              const viewportSlots = new Set([
                'home.hero.copy',
                'home.hero.media',
                'home.hero.divider'
              ]);
              [...hero.children]
                .filter(node => viewportSlots.has(node.getAttribute('data-ct-slot')))
                .forEach(node => heroViewport.appendChild(node));
              hero.prepend(heroViewport);
            }}
            if (hero.parentElement !== homeStage || hero !== homeStage.firstElementChild) {{
              homeStage.prepend(hero);
            }}
            const heroEyebrow = hero.querySelector('[data-ct-slot="home.hero.eyebrow"]');
            const heroTitle = hero.querySelector('[data-ct-slot="home.hero.title"]');
            const heroDescription = hero.querySelector('[data-ct-slot="home.hero.description"]');
            const heroDividerLabel = hero.querySelector(
              '[data-ct-slot="home.hero.divider.label"]'
            );
            if (heroEyebrow.textContent !== config.hero.eyebrow) {{
              heroEyebrow.textContent = config.hero.eyebrow;
            }}
            if (heroTitle.textContent !== config.hero.title) {{
              heroTitle.textContent = config.hero.title;
            }}
            if (heroDescription.textContent !== config.hero.description) {{
              heroDescription.textContent = config.hero.description;
            }}
            const dividerLabel = config.hero.divider?.label ?? '';
            if (heroDividerLabel.textContent !== dividerLabel) {{
              heroDividerLabel.textContent = dividerLabel;
            }}
            [
              'home.hero',
              'home.hero.viewport',
              'home.hero.copy',
              'home.hero.eyebrow',
              'home.hero.title',
              'home.hero.description',
              'home.hero.media',
              'home.hero.media.asset',
              'home.hero.foreground',
              'home.hero.foreground.asset',
              'home.hero.divider',
              'home.hero.divider.icon',
              'home.hero.divider.label',
              'home.hero.divider.line',
            ].forEach(slot => {{
              if (hero.matches(`[data-ct-slot="${{slot}}"]`) || hero.querySelector(`[data-ct-slot="${{slot}}"]`)) {{
                slots.push(slot);
              }}
            }});

            config.decorations.forEach(decoration => {{
              let mount = main.querySelector(`[data-ct-mount="${{decoration.slot}}"]`);
              if (!mount) {{
                mount = createMount(decoration.slot, 'ct-decoration');
                const image = document.createElement('img');
                image.alt = '';
                image.draggable = false;
                image.src = decoration.assetUrl;
                mount.appendChild(image);
                main.appendChild(mount);
              }}
              slots.push(decoration.slot);
            }});
          }};

          const collectMatches = (roots, selector) => [...new Set(
            roots.flatMap(root => {{
              if (!(root instanceof Element)) return [];
              return [
                ...(root.matches(selector) ? [root] : []),
                ...root.querySelectorAll(selector)
              ];
            }})
          )];

          const markConversationContent = (conversation, slots, roots = [conversation]) => {{
            if (!conversation) return;
            setSlot(conversation, 'conversation');
            if (!slots.includes('conversation')) slots.push('conversation');
            markSlot(
              collectMatches(roots, '[data-user-message-bubble="true"]'),
              'conversation.user',
              slots
            );
            markSlot(
              collectMatches(
                roots,
                '[data-local-conversation-final-assistant="true"], '
                  + '[data-content-search-unit-key$=":assistant"]'
              ),
              'conversation.assistant',
              slots
            );
            markSlot(collectMatches(roots, '[data-code], pre'), 'code', slots);
            markSlot(
              collectMatches(roots, '[data-markdown-copy="inline-code"]'),
              'code.inline',
              slots
            );
            markSlot(collectMatches(roots, '[data-diff]'), 'diff', slots);
            markSlot(collectMatches(roots, '[data-codex-terminal]'), 'terminal', slots);
            markSlot(
              collectMatches(roots, '[data-codex-xterm]'),
              'terminal.viewport',
              slots
            );
          }};

          let runtimeInstalled = false;
          const apply = () => {{
            const root = document.getElementById('root');
            if (!root || !document.documentElement) return null;
            if (!runtimeInstalled
              && !adapter.probes.every(selector => document.querySelector(selector))) return null;
            syncColorScheme();
            syncLocaleConfig();
            let style = document.getElementById('codex-theme-runtime-style');
            if (!style) {{
              style = document.createElement('style');
              style.id = 'codex-theme-runtime-style';
              document.documentElement.appendChild(style);
            }}
            const runtimeCss = {css} + '\n' + {platform_css};
            if (style.textContent !== runtimeCss) style.textContent = runtimeCss;
            document.documentElement.setAttribute('data-ct-theme', {theme_id});
            root.setAttribute('data-ct-slot', 'app.shell');
            document.querySelectorAll(
              '[data-ct-slot="page"], [data-ct-slot^="page."], '
                + '[data-ct-slot="menu"], [data-ct-slot^="menu."], '
                + '[data-ct-slot="settings"], [data-ct-slot^="settings."]'
            ).forEach(node => node.removeAttribute('data-ct-slot'));
            document.querySelectorAll(
              '[data-ct-slot="composer"], [data-ct-slot^="composer."]'
            ).forEach(node => {{
              if (!node.closest('[data-ct-mount]')) node.removeAttribute('data-ct-slot');
            }});
            document.querySelectorAll('[data-ct-workspace-panel]')
              .forEach(node => node.removeAttribute('data-ct-workspace-panel'));
            document.querySelectorAll('[data-ct-workspace-panel-region]')
              .forEach(node => node.removeAttribute('data-ct-workspace-panel-region'));
            const slots = ['app.shell', `adapter:${{adapter.id}}`];
            if (syncAssetMount(root, 'app.background', 'prepend')) {{
              slots.push('app.background');
            }}
            document.querySelectorAll('[data-ct-native-titlebar]')
              .forEach(node => node.removeAttribute('data-ct-native-titlebar'));
            const nativeTitlebars = [
              ...document.querySelectorAll(adapter.selectors.titlebar)
            ];
            const applicationMenus = adapter.selectors.applicationMenu
              ? [...document.querySelectorAll(adapter.selectors.applicationMenu)]
              : [];
            if (applicationMenus.length) {{
              nativeTitlebars.forEach(node => {{
                node.removeAttribute('data-ct-slot');
                node.setAttribute('data-ct-native-titlebar', '');
              }});
              markSlot(applicationMenus, 'titlebar', slots);
            }} else {{
              markSlot(nativeTitlebars, 'titlebar', slots);
            }}
            if (adapter.selectors.workspacePanel) {{
              document.querySelectorAll(adapter.selectors.workspacePanel).forEach(panel => {{
                panel.setAttribute('data-ct-workspace-panel', '');
                panel.parentElement?.setAttribute('data-ct-workspace-panel-region', '');
              }});
            }}
            if (adapter.selectors.mainContentFrame) {{
              markSlot(
                document.querySelectorAll(adapter.selectors.mainContentFrame),
                'main.content.frame',
                slots
              );
            }}
            const editors = [...document.querySelectorAll(adapter.selectors.composer)];
            const editor = editors.find(isVisible) ?? editors[0] ?? null;
            document.querySelectorAll('[data-ct-composer-layout]').forEach(node => {{
              if (node !== editor) node.removeAttribute('data-ct-composer-layout');
            }});
            const appMainCandidates = [...document.querySelectorAll(adapter.selectors.main)];
            const appMain = appMainCandidates.find(isVisible) ?? appMainCandidates[0] ?? null;
            const sidebarScrollCandidates = [
              ...document.querySelectorAll(adapter.selectors.sidebarScroll)
            ];
            const sidebarScroll = sidebarScrollCandidates.find(isVisible)
              ?? sidebarScrollCandidates[0]
              ?? null;
            if (sidebarScroll) {{
              sidebarScroll.setAttribute('data-ct-slot', 'sidebar.scroll');
              slots.push('sidebar.scroll');
              const sidebar = sidebarScroll.closest('aside');
              if (sidebar) {{
                sidebar.setAttribute('data-ct-slot', 'sidebar');
                slots.push('sidebar');
                sidebar.querySelectorAll(
                  '[data-ct-slot="sidebar.header"], '
                    + '[data-ct-slot="sidebar.header.icon"], '
                    + '[data-ct-slot="sidebar.header.label"], '
                    + '[data-ct-slot="sidebar.brand"], '
                    + '[data-ct-slot="sidebar.item"], '
                    + '[data-ct-slot="sidebar.item.icon"], '
                    + '[data-ct-slot="sidebar.item.label"], '
                    + '[data-ct-slot="sidebar.item.active"], '
                    + '[data-ct-slot="sidebar.item.active.icon"], '
                    + '[data-ct-slot="sidebar.item.active.label"], '
                    + '[data-ct-slot="sidebar.footer"], '
                    + '[data-ct-slot="sidebar.footer.item"], '
                    + '[data-ct-slot="sidebar.footer.icon"], '
                    + '[data-ct-slot="sidebar.footer.label"], '
                    + '[data-ct-slot="sidebar.footer.brand"], '
                    + '[data-ct-slot="sidebar.footer.brand.label"], '
                    + '[data-ct-slot="sidebar.footer.brand.timer"], '
                    + '[data-ct-slot="sidebar.footer.brand.pro"], '
                    + '[data-ct-slot="sidebar.footer.brand.version"]'
                ).forEach(node => {{
                  if (!node.closest('[data-ct-mount]')) node.removeAttribute('data-ct-slot');
                }});
                const sidebarSections = [...sidebar.querySelectorAll(
                  adapter.selectors.sidebarSection
                )];
                markSlot(sidebarSections, 'sidebar.section', slots);
                const projectSections = sidebarSections.filter(section => section.querySelector(
                  '[data-app-action-sidebar-project-list-id], '
                    + '[data-app-action-sidebar-project-row], '
                    + '[data-app-action-sidebar-select-project]'
                ));
                markSlot(projectSections, 'sidebar.section.projects', slots);
                sidebarSections.forEach(section => {{
                  const sectionBody = section.firstElementChild ?? section;
                  const toggle = section.querySelector('[data-app-action-sidebar-section-toggle]');
                  const header = toggle
                    ? [...sectionBody.children].find(node => node.contains(toggle))
                    : null;
                  if (!header) return;
                  header.setAttribute('data-ct-slot', 'sidebar.section.header');
                  toggle.setAttribute('data-ct-slot', 'sidebar.section.toggle');
                  const label = [...toggle.querySelectorAll('span')]
                    .find(node => node.textContent?.trim() && !node.querySelector('svg'));
                  if (label) label.setAttribute('data-ct-slot', 'sidebar.section.label');
                  const toggleGroup = [...header.children].find(node => node.contains(toggle));
                  const actions = [...header.children]
                    .find(node => node !== toggleGroup && node.querySelector('button'));
                  if (actions) {{
                    actions.setAttribute('data-ct-slot', 'sidebar.section.actions');
                    const actionButtons = [...actions.querySelectorAll('button')];
                    markSlot(actionButtons, 'sidebar.section.action', slots);
                    markSlot(
                      actionButtons.flatMap(button => [...button.querySelectorAll('svg')]),
                      'sidebar.section.action.icon',
                      slots
                    );
                  }}
                  if (projectSections.includes(section)
                    && label
                    && config.sidebarSectionDecoration?.assetUrl) {{
                    let decoration = toggle.querySelector(
                      ':scope > [data-ct-mount="sidebar.section.decoration"]'
                    );
                    if (!decoration) {{
                      decoration = createAssetMount(
                        'sidebar.section.decoration',
                        config.sidebarSectionDecoration.assetUrl
                      );
                      toggle.appendChild(decoration);
                    }}
                    slots.push('sidebar.section.decoration');
                  }}
                }});
                if (sidebarSections.length) {{
                  slots.push('sidebar.section.header', 'sidebar.section.toggle');
                }}
                if (sidebarSections.some(section => section.querySelector('[data-ct-slot="sidebar.section.label"]'))) {{
                  slots.push('sidebar.section.label');
                }}
                if (sidebarSections.some(section => section.querySelector('[data-ct-slot="sidebar.section.actions"]'))) {{
                  slots.push('sidebar.section.actions');
                }}
                const itemCandidates = [...new Set([
                  ...sidebar.querySelectorAll(
                    'button, a, [role="button"], '
                      + '[data-app-action-sidebar-project-row], '
                      + '[data-app-action-sidebar-thread-row], '
                      + '[data-app-action-sidebar-select-project]'
                  )
                ])].filter(item =>
                  !item.closest('[data-ct-slot="sidebar.section.header"]')
                  && !item.matches('[data-ct-slot="sidebar.section.action"]')
                );
                markSlot(itemCandidates, 'sidebar.item', slots);
                markSlot(
                  itemCandidates.flatMap(item => [...item.querySelectorAll('svg')]),
                  'sidebar.item.icon',
                  slots
                );
                markSlot(
                  itemCandidates.flatMap(item => [...item.querySelectorAll('span')])
                    .filter(label => label.textContent?.trim() && !label.querySelector('svg')),
                  'sidebar.item.label',
                  slots
                );
                const outsideScrollItems = itemCandidates.filter(item => !sidebarScroll.contains(item));
                const boundaryRegion = item => {{
                  let region = item;
                  while (region.parentElement
                    && region.parentElement !== sidebar
                    && region.parentElement !== sidebarScroll) {{
                    region = region.parentElement;
                  }}
                  return region;
                }};
                const footerItem = outsideScrollItems
                  .reduce((current, item) => !current
                    || item.getBoundingClientRect().bottom > current.getBoundingClientRect().bottom
                    ? item : current, null);
                const findBottomRegion = item => {{
                  const sidebarRect = sidebar.getBoundingClientRect();
                  let region = item;
                  while (region && region !== sidebar && region !== sidebarScroll) {{
                    const rect = region.getBoundingClientRect();
                    const style = getComputedStyle(region);
                    if (style.position === 'absolute'
                      && Math.abs(rect.bottom - sidebarRect.bottom) <= 4
                      && rect.width >= sidebarRect.width * 0.8) {{
                      return region;
                    }}
                    region = region.parentElement;
                  }}
                  return null;
                }};
                const footer = footerItem ? findBottomRegion(footerItem) : null;
                const headerItem = outsideScrollItems
                  .filter(item => !footer?.contains(item))
                  .reduce((current, item) => !current
                    || item.getBoundingClientRect().top < current.getBoundingClientRect().top
                    ? item : current, null);
                const findHeaderRegion = item => {{
                  const sidebarRect = sidebar.getBoundingClientRect();
                  let region = item;
                  let matched = null;
                  while (region && region !== sidebar && region !== sidebarScroll) {{
                    const rect = region.getBoundingClientRect();
                    if (rect.width >= sidebarRect.width * 0.8 && rect.height <= 100) matched = region;
                    if (rect.height > 100) break;
                    region = region.parentElement;
                  }}
                  return matched ?? item;
                }};
                if (headerItem) {{
                  const header = findHeaderRegion(headerItem);
                  markSlot([header], 'sidebar.header', slots);
                  markSlot(headerItem.querySelectorAll('svg'), 'sidebar.header.icon', slots);
                  markSlot(
                    [...headerItem.querySelectorAll('span')]
                      .filter(label => label.textContent?.trim() && !label.querySelector('svg')),
                    'sidebar.header.label',
                    slots
                  );
                  headerItem.setAttribute('data-ct-slot', 'sidebar.brand');
                  slots.push('sidebar.brand');
                  for (const slot of ['sidebar.brand.icon', 'sidebar.brand.badge']) {{
                    if (syncAssetMount(headerItem, slot)) slots.push(slot);
                  }}
                  if (syncAssetMount(header, 'sidebar.header.background', 'prepend')) {{
                    slots.push('sidebar.header.background');
                  }}
                  if (syncAssetMount(header, 'sidebar.header.decoration', 'prepend')) {{
                    slots.push('sidebar.header.decoration');
                  }}
                }}
                if (footer && footerItem !== headerItem) {{
                  markSlot([footer], 'sidebar.footer', slots);
                  const footerItems = itemCandidates.filter(item => footer.contains(item));
                  markSlot(footerItems, 'sidebar.footer.item', slots);
                  markSlot(
                    footerItems.flatMap(item => [...item.querySelectorAll('svg')]),
                    'sidebar.footer.icon',
                    slots
                  );
                  markSlot(
                    footerItems.flatMap(item => [...item.querySelectorAll('span')])
                      .filter(label => label.textContent?.trim() && !label.querySelector('svg')),
                    'sidebar.footer.label',
                    slots
                  );
                  let brand = sidebar.querySelector('[data-ct-mount="sidebar.footer.brand"]');
                  if (brand && brand.parentElement !== footer) {{
                    brand.remove();
                    brand = null;
                  }}
                  if (!brand) {{
                    brand = createMount('sidebar.footer.brand', 'ct-sidebar-footer-brand');
                    brand.setAttribute('aria-hidden', 'true');
                    footer.prepend(brand);
                  }}
                  let label = brand.querySelector('[data-ct-slot="sidebar.footer.brand.label"]');
                  if (!label) {{
                    label = document.createElement('span');
                    label.dataset.ctSlot = 'sidebar.footer.brand.label';
                    label.textContent = 'ReTheme';
                    brand.prepend(label);
                  }}
                  let pro = brand.querySelector('[data-ct-slot="sidebar.footer.brand.pro"]');
                  let timer = brand.querySelector('[data-ct-slot="sidebar.footer.brand.timer"]');
                  let version = brand.querySelector('[data-ct-slot="sidebar.footer.brand.version"]');
                  if (hasPro) {{
                    timer?.remove();
                    version?.remove();
                    if (!pro) {{
                      pro = document.createElement('span');
                      pro.dataset.ctSlot = 'sidebar.footer.brand.pro';
                      pro.textContent = 'PRO';
                      brand.appendChild(pro);
                    }}
                  }} else if (hardExpiresAt) {{
                    pro?.remove();
                    version?.remove();
                    if (!timer) {{
                      timer = document.createElement('span');
                      timer.dataset.ctSlot = 'sidebar.footer.brand.timer';
                      brand.appendChild(timer);
                    }}
                    syncThemeStatus();
                  }} else {{
                    timer?.remove();
                    pro?.remove();
                    if (!version) {{
                      version = document.createElement('span');
                      version.dataset.ctSlot = 'sidebar.footer.brand.version';
                      brand.appendChild(version);
                    }}
                    version.textContent = `v${{themeVersion}}`;
                  }}
                  slots.push('sidebar.footer.brand', 'sidebar.footer.brand.label');
                  if (hasPro) slots.push('sidebar.footer.brand.pro');
                  else if (hardExpiresAt) slots.push('sidebar.footer.brand.timer');
                  else slots.push('sidebar.footer.brand.version');
                  let heightOwner = footer.parentElement;
                  while (heightOwner && heightOwner !== sidebar.parentElement) {{
                    const currentHeight = heightOwner.style.getPropertyValue(
                      '--sidebar-footer-height'
                    );
                    if (currentHeight) {{
                      if (!heightOwner.hasAttribute('data-ct-sidebar-footer-height')) {{
                        heightOwner.setAttribute('data-ct-sidebar-footer-height', currentHeight);
                      }}
                      heightOwner.style.setProperty('--sidebar-footer-height', '78px');
                      break;
                    }}
                    heightOwner = heightOwner.parentElement;
                  }}
                }}
                const activeItems = [...sidebar.querySelectorAll(
                  '[aria-current="page"], [data-app-action-sidebar-thread-active="true"]'
                )];
                activeItems.forEach(item => {{
                  item.setAttribute('data-ct-slot', 'sidebar.item.active');
                  const icon = item.querySelector('svg');
                  if (icon) icon.setAttribute('data-ct-slot', 'sidebar.item.active.icon');
                  [...item.querySelectorAll('span')]
                    .filter(label => label.textContent?.trim() && !label.querySelector('svg'))
                    .forEach(label => label.setAttribute('data-ct-slot', 'sidebar.item.active.label'));
                }});
                if (activeItems.length) slots.push('sidebar.item.active');
                if (activeItems.some(item => item.querySelector('[data-ct-slot="sidebar.item.active.icon"]'))) {{
                  slots.push('sidebar.item.active.icon');
                }}
                if (activeItems.some(item => item.querySelector('[data-ct-slot="sidebar.item.active.label"]'))) {{
                  slots.push('sidebar.item.active.label');
                }}
                const sidebarResize = [...sidebar.children].find(node =>
                  node.matches('[role="separator"][aria-orientation="vertical"]')
                );
                if (sidebarResize) {{
                  sidebarResize.setAttribute('data-ct-slot', 'sidebar.resize');
                  slots.push('sidebar.resize');
                  const resizeIndicator = [...sidebarResize.children].find(node =>
                    getComputedStyle(node).pointerEvents === 'none'
                  );
                  if (resizeIndicator) {{
                    resizeIndicator.setAttribute('data-ct-slot', 'sidebar.resize.indicator');
                    slots.push('sidebar.resize.indicator');
                  }}
                }}
                if (syncAssetMount(sidebar, 'sidebar.frame')) slots.push('sidebar.frame');
              }}
            }}
            if (appMain) {{
              appMain.setAttribute('data-ct-slot', 'main');
              slots.push('main');
              if (adapter.selectors.mainTopFade) {{
                markSlot(
                  document.querySelectorAll(adapter.selectors.mainTopFade),
                  'main.fade',
                  slots
                );
              }}
              for (const slot of ['main.background', 'main.overlay', 'main.frame']) {{
                if (syncAssetMount(appMain, slot, slot === 'main.background' ? 'prepend' : 'append')) {{
                  slots.push(slot);
                }}
              }}
              syncTitlebarSafeTop(appMain);
              const visibleConversation = [...document.querySelectorAll(
                adapter.selectors.conversation
              )].find(isVisible);
              const pageAnchor = visibleConversation
                ?? (isVisible(editor) ? editor : appMain.firstElementChild);
              mountHero(appMain, editor, slots);
              if (!['home', 'home-compact', 'conversation'].includes(
                document.documentElement.getAttribute('data-ct-view')
              )) {{
                markPage(appMain, pageAnchor, slots);
              }}
            }}
            if (editor) {{
              const composerRoot = editor.closest(adapter.selectors.composerRoot);
              const editorSurface = editor;
              const composer = composerRoot
                ? [...composerRoot.querySelectorAll('*')].find(node => {{
                    const style = getComputedStyle(node);
                    return node.contains(editor)
                      && node.querySelector('button')
                      && (style.backgroundColor !== 'rgba(0, 0, 0, 0)'
                        || style.backdropFilter !== 'none');
                  }})
                : editor.closest('form');
              if (composer) {{
                composer.setAttribute('data-ct-slot', 'composer');
                slots.push('composer');
              }}
              let composerSticky = composer?.parentElement ?? null;
              while (
                composerSticky
                && composerSticky !== appMain
                && getComputedStyle(composerSticky).position !== 'sticky'
              ) {{
                composerSticky = composerSticky.parentElement;
              }}
              const backdrop = document.documentElement.dataset.ctView === 'conversation'
                && composerSticky
                && getComputedStyle(composerSticky).position === 'sticky'
                ? [...composerSticky.children].find(node => {{
                    if (node.contains(composer)) return false;
                    const style = getComputedStyle(node);
                    return style.position === 'absolute'
                      && style.pointerEvents === 'none'
                      && [node, ...node.querySelectorAll('*')].some(child =>
                        getComputedStyle(child).backgroundImage.includes('gradient')
                      );
                  }})
                : null;
              if (backdrop) {{
                backdrop.setAttribute('data-ct-slot', 'composer.backdrop');
                slots.push('composer.backdrop');
              }}
              if (editorSurface) {{
                editorSurface.setAttribute('data-ct-slot', 'composer.editor');
                syncComposerLayout(editorSurface);
                observeComposerLayout(editorSurface);
                slots.push('composer.editor');
              }}
              composerRoot?.querySelectorAll(
                '[data-ct-slot="composer.context"], '
                  + '[data-ct-slot="composer.context.item"], '
                  + '[data-ct-slot="composer.context.item.icon"], '
                  + '[data-ct-slot="composer.context.item.label"]'
              ).forEach(node => node.removeAttribute('data-ct-slot'));
              const utilityBar = adapter.selectors.composerUtilityBar
                ? composerRoot?.querySelector(adapter.selectors.composerUtilityBar)
                : null;
              const utilityBarSurface = utilityBar?.parentElement ?? null;
              let contextSlot = null;
              if (utilityBarSurface) {{
                contextSlot = utilityBarSurface;
              }} else {{
                const clearProjectButton = composerRoot?.querySelector(
                  '[data-clear-project-button]'
                );
                if (clearProjectButton) {{
                let context = clearProjectButton.parentElement;
                while (context && context !== composerRoot) {{
                  const siblings = [...(context.parentElement?.children ?? [])];
                  if (siblings.some(sibling => sibling !== context && sibling.contains(editor))) {{
                    let contextSurface = clearProjectButton.parentElement;
                    while (contextSurface && contextSurface !== context) {{
                      const style = getComputedStyle(contextSurface);
                      if (style.backgroundColor !== 'rgba(0, 0, 0, 0)'
                        && parseFloat(style.borderTopLeftRadius) > 0) break;
                      contextSurface = contextSurface.parentElement;
                    }}
                    contextSlot = contextSurface && contextSurface !== context
                      ? contextSurface
                      : context;
                    break;
                  }}
                  context = context.parentElement;
                }}
                }}
              }}
              if (contextSlot) {{
                markSlot([contextSlot], 'composer.context', slots);
                const contextItems = [...contextSlot.querySelectorAll(
                  'button, [role="button"]'
                )];
                markSlot(contextItems, 'composer.context.item', slots);
                markSlot(
                  contextItems.flatMap(item => [...item.querySelectorAll('svg')]),
                  'composer.context.item.icon',
                  slots
                );
                markSlot(
                  contextItems.flatMap(item => [...item.querySelectorAll('span')])
                    .filter(label => label.textContent?.trim() && !label.querySelector('svg')),
                  'composer.context.item.label',
                  slots
                );
              }}
              const composerButtons = [...(composerRoot?.querySelectorAll('button') ?? [])]
                .filter(button => !contextSlot?.contains(button));
              markSlot(composerButtons, 'composer.action', slots);
              markSlot(
                composerButtons.flatMap(button => [...button.querySelectorAll('svg')]),
                'composer.action.icon',
                slots
              );
              markSlot(
                composerButtons.flatMap(button => [...button.querySelectorAll('span')])
                  .filter(label => label.textContent?.trim() && !label.querySelector('svg')),
                'composer.action.label',
                slots
              );
              const permissionButton = composerRoot?.querySelector(
                '[data-composer-navigation-target="permissions"]'
              );
              if (permissionButton) {{
                markSlot([permissionButton], 'composer.permission', slots);
                markSlot(
                  permissionButton.querySelectorAll('svg'),
                  'composer.permission.icon',
                  slots
                );
                markSlot(
                  [...permissionButton.querySelectorAll('span')]
                    .filter(label => label.textContent?.trim() && !label.querySelector('svg')),
                  'composer.permission.label',
                  slots
                );
              }}
              const submitButton = composerRoot?.querySelector('button[type="submit"]')
                ?? composerButtons.reverse().find(button =>
                  button.querySelector('svg')
                  && !button.hasAttribute('data-composer-navigation-target')
                  && !button.hasAttribute('data-clear-project-button')
                  && Boolean(editor.compareDocumentPosition(button) & Node.DOCUMENT_POSITION_FOLLOWING)
                );
              if (submitButton) {{
                markSlot([submitButton], 'composer.submit', slots);
                markSlot(submitButton.querySelectorAll('svg'), 'composer.submit.icon', slots);
                if (config.composerSubmit?.assetUrl) {{
                  let decoration = submitButton.querySelector(
                    ':scope > [data-ct-mount="composer.submit.decoration"]'
                  );
                  if (!decoration) {{
                    decoration = createAssetMount(
                      'composer.submit.decoration',
                      config.composerSubmit.assetUrl
                    );
                    submitButton.appendChild(decoration);
                  }}
                  slots.push('composer.submit.decoration');
                }}
              }}
              if (composer && config.composerDecoration?.assetUrl) {{
                let decoration = composer.querySelector(
                  ':scope > [data-ct-mount="composer.decoration"]'
                );
                if (!decoration) {{
                  decoration = createAssetMount(
                    'composer.decoration',
                    config.composerDecoration.assetUrl
                  );
                  composer.appendChild(decoration);
                }}
                slots.push('composer.decoration');
              }}
              const panelTrigger = composerRoot?.querySelector(
                '[aria-expanded="true"][aria-controls]'
              );
              const composerPanel = panelTrigger
                ? document.getElementById(panelTrigger.getAttribute('aria-controls'))
                : null;
              if (composerPanel) {{
                markSlot([composerPanel], 'composer.panel', slots);
                const panelItems = composerPanel.querySelectorAll('button, [role="button"], [role="option"]');
                markSlot(panelItems, 'composer.panel.item', slots);
                markSlot(
                  [...panelItems].flatMap(item => [...item.querySelectorAll('svg')]),
                  'composer.panel.icon',
                  slots
                );
                markSlot(
                  composerPanel.querySelectorAll('hr, [role="separator"]'),
                  'composer.panel.separator',
                  slots
                );
              }}
            }}

            const conversation = [...document.querySelectorAll(
              adapter.selectors.conversation
            )].find(isVisible);
            if (conversation) {{
              document.querySelectorAll('[data-ct-slot="conversation"]')
                .forEach(node => {{
                  if (node !== conversation) node.removeAttribute('data-ct-slot');
                }});
              markConversationContent(conversation, slots);
            }}

            const settingsItems = [...document.querySelectorAll(adapter.selectors.settingsItem)];
            markSettings(settingsItems, appMain, slots);
            markMenus(slots);
            const hero = document.querySelector('[data-ct-mount="home.hero"]');
            const main = document.querySelector('[data-ct-slot="main"]');
            const titlebar = [...document.querySelectorAll('[data-ct-slot="titlebar"]')]
              .filter(isVisible)
              .reduce((visible, candidate) => !visible
                || candidate.getBoundingClientRect().bottom > visible.getBoundingClientRect().bottom
                ? candidate : visible, null);
            const composer = document.querySelector('[data-ct-slot="composer"]');
            const composerEditor = document.querySelector('[data-ct-slot="composer.editor"]');
            const homeBrand = document.querySelector('[data-ct-slot="home.brand"]');
            const homeStage = document.querySelector('[data-ct-slot="home.stage"]');
            const homeSource = document.querySelector(adapter.selectors.homeSource);
            const homeCards = [...document.querySelectorAll('[data-ct-slot="home.card"]')];
            const homeCardsGrid = document.querySelector('[data-ct-slot="home.cards.grid"]');
            const submitDecoration = document.querySelector(
              '[data-ct-mount="composer.submit.decoration"]'
            );
            const composerDecoration = document.querySelector(
              '[data-ct-mount="composer.decoration"]'
            );
            const sidebarDecoration = document.querySelector(
              '[data-ct-mount="sidebar.section.decoration"]'
            );
            const currentView = document.documentElement.getAttribute('data-ct-view') ?? 'other';
            slots.push(`view:${{currentView}}`);
            if (currentView !== 'home') {{
              return [...new Set(slots)];
            }}
            if (!hero || !main || !titlebar || !composer || !composerEditor || !homeStage || !homeSource || !homeCards.length) return null;
            const heroRect = hero.getBoundingClientRect();
            const mainRect = main.getBoundingClientRect();
            const titlebarRect = titlebar.getBoundingClientRect();
            const composerRect = composer.getBoundingClientRect();
            const editorRect = composerEditor.getBoundingClientRect();
            const composerStyle = getComputedStyle(composer);
            const editorStyle = getComputedStyle(composerEditor);
            const heroVisible = heroRect.width >= 320
              && heroRect.height >= 200
              && heroRect.left >= mainRect.left
              && heroRect.right <= mainRect.right
              && heroRect.top >= titlebarRect.bottom;
            const composerStyled = parseFloat(composerStyle.borderTopLeftRadius) > 0
              && composerStyle.backgroundColor !== 'rgba(0, 0, 0, 0)';
            const editorRadius = parseFloat(editorStyle.borderTopLeftRadius) > 0;
            const editorFilled = editorStyle.backgroundColor !== 'rgba(0, 0, 0, 0)';
            const editorTallEnough = editorRect.height >= 64;
            const editorPadded = parseFloat(editorStyle.paddingLeft) >= 12
              && parseFloat(editorStyle.paddingRight) >= 12
              && parseFloat(editorStyle.paddingTop) >= 12
              && parseFloat(editorStyle.paddingBottom) >= 12;
            const editorInsetLeft = editorRect.left > composerRect.left;
            const editorInsetRight = editorRect.right < composerRect.right;
            const editorInsetTop = editorRect.top > composerRect.top;
            const brandHidden = !homeBrand || getComputedStyle(homeBrand).display === 'none';
            const homeFitsViewport = homeStage.scrollWidth <= homeStage.clientWidth + 2;
            const cardWidths = homeCards.map(card => card.getBoundingClientRect().width);
            const cardsEvenlyDistributed = Math.max(...cardWidths) - Math.min(...cardWidths) <= 2;
            const cardsGridRect = homeCardsGrid?.getBoundingClientRect();
            const cardRowBounds = homeCards.reduce((bounds, card) => {{
              const rect = card.getBoundingClientRect();
              if (Math.abs(rect.top - bounds.top) > 2) return bounds;
              return {{ top: bounds.top, left: Math.min(bounds.left, rect.left), right: Math.max(bounds.right, rect.right) }};
            }}, {{
              top: homeCards[0].getBoundingClientRect().top,
              left: Number.POSITIVE_INFINITY,
              right: Number.NEGATIVE_INFINITY
            }});
            const cardsFillRow = cardsGridRect
              && Math.abs(cardRowBounds.left - cardsGridRect.left) <= 2
              && Math.abs(cardRowBounds.right - cardsGridRect.right) <= 2;
            const visualVisible = node => {{
              if (!node) return false;
              const rect = node.getBoundingClientRect();
              const style = getComputedStyle(node);
              return rect.width >= 16
                && rect.height >= 16
                && style.display !== 'none'
                && style.visibility !== 'hidden'
                && parseFloat(style.opacity) > 0;
            }};
            const composerDecorationOffset = !composerDecoration
              || composerDecoration.getBoundingClientRect().bottom > composerRect.bottom;
            const cardsClearBanner = Math.min(
              ...homeCards.map(card => card.getBoundingClientRect().top)
            ) >= heroRect.bottom - 2;
            const cardsClearComposer = Math.max(
              ...homeCards.map(card => card.getBoundingClientRect().bottom)
            ) <= composerRect.top - 16;
            const embeddedCardsInteractive = homeCards.every(card => {{
              const rect = card.getBoundingClientRect();
              if (rect.width < 120
                || rect.height < 80
                || rect.left < heroRect.left - 2
                || rect.right > heroRect.right + 2
                || rect.top < heroRect.top - 2
                || rect.bottom > heroRect.bottom + 2) return false;
              const target = document.elementFromPoint(
                rect.left + rect.width / 2,
                rect.top + rect.height / 2
              );
              return target?.closest('[data-ct-slot="home.card"]') === card;
            }});
            if (!heroVisible) slots.push('health.hero.invisible');
            if (!cardsClearBanner && !embeddedCardsInteractive) {{
              const firstCardRect = homeCards[0].getBoundingClientRect();
              const lastCardRect = homeCards.at(-1).getBoundingClientRect();
              slots.push(`health.hero.layout:${{[
                heroRect.left,
                heroRect.top,
                heroRect.right,
                heroRect.bottom,
                firstCardRect.left,
                firstCardRect.top,
                lastCardRect.right,
                lastCardRect.bottom
              ].map(value => Math.round(value)).join('/') }}`);
            }}
            if (!brandHidden) slots.push('health.home-brand.visible');
            if (!homeFitsViewport) slots.push('health.home.horizontal-overflow');
            if (!cardsEvenlyDistributed || !cardsFillRow) slots.push('health.home-cards.distribution');
            if (!cardsClearComposer) slots.push('health.home-cards.composer-overlap');
            if (config.composerSubmit && !visualVisible(submitDecoration)) slots.push('health.composer-submit.decoration');
            if (config.composerDecoration && (!visualVisible(composerDecoration) || !composerDecorationOffset)) slots.push('health.composer.decoration');
            if (config.sidebarSectionDecoration && !visualVisible(sidebarDecoration)) slots.push('health.sidebar-section.decoration');
            if (!composerStyled) slots.push('health.composer.unstyled');
            if (!editorRadius) slots.push('health.composer-editor.radius');
            if (!editorFilled) slots.push('health.composer-editor.fill');
            if (!editorTallEnough) slots.push('health.composer-editor.height');
            if (!editorPadded) slots.push('health.composer-editor.padding');
            if (!editorInsetLeft) slots.push('health.composer-editor.left');
            if (!editorInsetRight) slots.push('health.composer-editor.right');
            if (!editorInsetTop) slots.push('health.composer-editor.top');
            return [...new Set(slots)];
          }};

          const slots = apply();
          if (!slots) return null;
          const observeOptions = {{
            childList: true,
            subtree: true,
            attributes: true,
            attributeFilter: [
              'aria-current',
              'aria-checked',
              'aria-expanded',
              'data-content-search-unit-key',
              'data-highlighted',
              'data-local-conversation-final-assistant',
              'data-state',
              'data-app-action-sidebar-thread-active',
              'data-user-message-bubble'
            ]
          }};
          const runtime = {{
            sessionId,
            slots,
            revokeAssets,
            hardExpiresAt,
            leaseExpiresAt: 0,
            leaseTimer: 0,
            syncThemeStatus,
            syncLocale: null,
            restoreTheme,
            observer: null,
            schemeObserver: null,
            composerLayoutObserver,
            colorSchemeMedia,
            syncColorScheme,
            frame: 0,
            homeFrame: 0,
            contentFrame: 0,
            metricsFrame: 0,
            resizeFrame: 0,
            apply,
            root: document.getElementById('root'),
            eventTarget: document,
            pendingConversationRoots: new Set(),
            lastApplyError: null,
            handleInput: null,
            handleNavigation: null,
            handleResize: null
          }};
          const scheduleApply = () => {{
            if (runtime.frame) return;
            runtime.frame = requestAnimationFrame(() => {{
              runtime.frame = 0;
              if (runtime.homeFrame) cancelAnimationFrame(runtime.homeFrame);
              if (runtime.contentFrame) cancelAnimationFrame(runtime.contentFrame);
              if (runtime.metricsFrame) cancelAnimationFrame(runtime.metricsFrame);
              runtime.homeFrame = 0;
              runtime.contentFrame = 0;
              runtime.metricsFrame = 0;
              runtime.pendingConversationRoots.clear();
              runtime.observer.disconnect();
              try {{
                runtime.root = document.getElementById('root');
                runtime.slots = apply() ?? runtime.slots;
                runtime.lastApplyError = null;
              }} catch (error) {{
                runtime.lastApplyError = {{
                  message: String(error),
                  stack: error?.stack ?? null
                }};
              }} finally {{
                if (window[runtimeKey] === runtime && document.documentElement) {{
                  runtime.observer.observe(document.documentElement, observeOptions);
                }}
              }}
            }});
          }};
          const scheduleHomeRefresh = () => {{
            if (runtime.frame || runtime.homeFrame) return;
            runtime.homeFrame = requestAnimationFrame(() => {{
              runtime.homeFrame = 0;
              const view = document.documentElement.dataset.ctView;
              if (view !== 'home' && view !== 'home-compact') return;
              const appMain = [...document.querySelectorAll(adapter.selectors.main)]
                .find(isVisible);
              const editors = [...document.querySelectorAll(adapter.selectors.composer)];
              const editor = editors.find(isVisible) ?? editors[0] ?? null;
              if (!appMain || !editor) return;
              runtime.observer.disconnect();
              try {{
                const homeSlots = [];
                mountHero(appMain, editor, homeSlots);
                syncComposerLayout(editor);
                syncTitlebarSafeTop(appMain);
                runtime.slots = [...new Set([...runtime.slots, ...homeSlots])];
                runtime.lastApplyError = null;
              }} catch (error) {{
                runtime.lastApplyError = {{
                  message: String(error),
                  stack: error?.stack ?? null
                }};
              }} finally {{
                if (window[runtimeKey] === runtime && document.documentElement) {{
                  runtime.observer.observe(document.documentElement, observeOptions);
                }}
              }}
            }});
          }};
          runtime.syncLocale = locale => {{
            requestedLocale = locale;
            syncLocaleConfig();
            runtime.slots = apply() ?? runtime.slots;
            return true;
          }};
          const scheduleConversationContent = roots => {{
            roots.forEach(root => {{
              if (root instanceof Element) runtime.pendingConversationRoots.add(root);
            }});
            if (runtime.contentFrame) return;
            runtime.contentFrame = requestAnimationFrame(() => {{
              runtime.contentFrame = 0;
              const conversation = [...document.querySelectorAll(
                adapter.selectors.conversation
              )].find(isVisible);
              const contentRoots = [...runtime.pendingConversationRoots]
                .filter(root => root.isConnected && conversation?.contains(root));
              runtime.pendingConversationRoots.clear();
              if (!conversation || !contentRoots.length) return;
              const contentSlots = [];
              markConversationContent(conversation, contentSlots, contentRoots);
              runtime.slots = [...new Set([...runtime.slots, ...contentSlots])];
            }});
          }};
          const scheduleMetrics = () => {{
            if (runtime.metricsFrame) return;
            runtime.metricsFrame = requestAnimationFrame(() => {{
              runtime.metricsFrame = 0;
              const appMain = [...document.querySelectorAll(adapter.selectors.main)]
                .find(isVisible);
              syncComposerLayout(
                document.querySelector('[data-ct-slot="composer.editor"]')
              );
              syncTitlebarSafeTop(appMain);
              syncConversationHeaderLayout();
              syncConversationBannerForeground();
              const stage = document.querySelector('[data-ct-slot="conversation.stage"]');
              if (!stage) return;
              const metricSlots = [];
              markConversationSummary(stage, metricSlots);
              runtime.slots = [
                ...new Set([
                  ...runtime.slots.filter(slot =>
                    !['conversation.summary.region', 'conversation.summary'].includes(slot)
                  ),
                  ...metricSlots
                ])
              ];
            }});
          }};
          const structuralSelectors = [
            adapter.selectors.main,
            adapter.selectors.composerRoot,
            adapter.selectors.conversation,
            adapter.selectors.conversationSummaryRegion,
            adapter.selectors.homeSource,
            adapter.selectors.settingsItem,
            adapter.selectors.sidebarScroll,
            adapter.selectors.workspacePanel,
            '[role="menu"]'
          ].filter(Boolean);
          const touchesStructure = node => node instanceof Element
            && structuralSelectors.some(selector =>
              node.matches(selector) || Boolean(node.querySelector(selector))
            );
          runtime.observer = new MutationObserver(mutations => {{
            let needsApply = false;
            let needsHomeRefresh = false;
            const conversationRoots = [];
            const settingsOpen = Boolean(
              document.querySelector(adapter.selectors.settingsItem)
            );
            for (const mutation of mutations) {{
              const target = mutation.target instanceof Element ? mutation.target : null;
              const conversation = target?.closest(adapter.selectors.conversation);
              if (mutation.type === 'attributes') {{
                if (conversation) {{
                  conversationRoots.push(target);
                  continue;
                }}
                const view = document.documentElement.dataset.ctView;
                if ((view === 'home' || view === 'home-compact')
                  && target?.closest(adapter.selectors.composerRoot)) {{
                  needsHomeRefresh = true;
                  continue;
                }}
                if (target?.closest([
                  'aside',
                  adapter.selectors.composerRoot,
                  adapter.selectors.settingsItem,
                  adapter.selectors.workspacePanel,
                  '[role="menu"]'
                ].filter(Boolean).join(', '))) {{
                  needsApply = true;
                }}
                continue;
              }}
              const changedNodes = [
                ...mutation.addedNodes,
                ...mutation.removedNodes
              ];
              if (changedNodes.some(touchesStructure)
                || target === runtime.root
                || target === document.body
                || target === document.documentElement) {{
                needsApply = true;
                continue;
              }}
              if (conversation) {{
                conversationRoots.push(
                  ...mutation.addedNodes,
                  ...(target instanceof Element ? [target] : [])
                );
                continue;
              }}
              if (settingsOpen && target?.closest(adapter.selectors.main)) {{
                needsApply = true;
                continue;
              }}
              const view = document.documentElement.dataset.ctView;
              if ((view === 'home' || view === 'home-compact')
                && target?.closest(adapter.selectors.main)) {{
                needsHomeRefresh = true;
                continue;
              }}
              if (target?.closest([
                'aside',
                adapter.selectors.composerRoot,
                adapter.selectors.settingsItem,
                adapter.selectors.workspacePanel,
                '[role="menu"]'
              ].filter(Boolean).join(', '))) {{
                needsApply = true;
              }}
            }}
            if (conversationRoots.length) scheduleConversationContent(conversationRoots);
            if (needsApply) scheduleApply();
            else if (needsHomeRefresh) scheduleHomeRefresh();
          }});
          runtime.handleInput = event => {{
            const composerRoot = document.querySelector(adapter.selectors.composerRoot);
            const view = document.documentElement.dataset.ctView;
            if ((view === 'home' || view === 'home-compact')
              && event.target instanceof Node
              && composerRoot?.contains(event.target)) {{
              scheduleHomeRefresh();
            }}
          }};
          runtime.handleNavigation = event => {{
            if (!(event.target instanceof Element)) return;
            if (event.target.closest(
              '[data-app-action-sidebar-thread-row], '
                + '[data-app-action-sidebar-select-thread], '
                + '[data-settings-panel-slug]'
            )) {{
              requestAnimationFrame(() => requestAnimationFrame(scheduleApply));
            }}
          }};
          runtime.handleResize = () => {{
            scheduleMetrics();
            if (runtime.resizeFrame) cancelAnimationFrame(runtime.resizeFrame);
            runtime.resizeFrame = requestAnimationFrame(() => {{
              runtime.resizeFrame = 0;
              scheduleMetrics();
            }});
          }};
          runtime.observer.observe(document.documentElement, observeOptions);
          runtime.eventTarget.addEventListener('input', runtime.handleInput, true);
          runtime.eventTarget.addEventListener('click', runtime.handleNavigation, true);
          window.addEventListener('resize', runtime.handleResize);
          runtime.schemeObserver = new MutationObserver(mutations => {{
            syncColorScheme();
          }});
          runtime.schemeObserver.observe(document.documentElement, {{
            attributes: true,
            attributeFilter: ['class']
          }});
          colorSchemeMedia.addEventListener('change', syncColorScheme);
          runtimeInstalled = true;
          window[runtimeKey] = runtime;
          installPageLease(runtimeKey, runtime, pageLeaseMilliseconds);
          scheduleApply();
          return slots;
        }})()"#
    );

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    socket
        .get_mut()
        .set_read_timeout(Some(THEME_CHANNEL_READ_POLL_INTERVAL))
        .map_err(|error| CodexError(format!("无法配置本地主题通道响应等待：{error}")))?;
    let mut command_id = 2;
    let mut last_slots = Vec::new();
    let slots = loop {
        let value = evaluate_value_until(&mut socket, command_id, &expression, deadline)?;
        if let Some(slots) = value.as_array() {
            let slots: Vec<String> = slots
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            last_slots.clone_from(&slots);
            if startup_slots_ready(&slots) {
                break slots;
            }
        }
        if Instant::now() >= deadline {
            let missing = missing_startup_slots(&last_slots);
            return Err(CodexError(format!(
                "等待 ChatGPT 主题界面就绪超时，缺少核心区域：{}",
                missing.join(", ")
            )));
        }
        command_id += 1;
        thread::sleep(Duration::from_millis(100));
    };
    let _ = socket.close(None);
    Ok(slots)
}

fn startup_slots_ready(slots: &[String]) -> bool {
    missing_startup_slots(slots).is_empty()
}

fn missing_startup_slots(slots: &[String]) -> Vec<&'static str> {
    let view = slots.iter().find_map(|slot| slot.strip_prefix("view:"));
    let view_slots = match view {
        Some("home") => REQUIRED_HOME_STARTUP_SLOTS,
        Some("home-compact") => REQUIRED_COMPACT_HOME_STARTUP_SLOTS,
        Some("conversation") => REQUIRED_CONVERSATION_STARTUP_SLOTS,
        _ => return vec![REQUIRED_STARTUP_VIEW],
    };
    REQUIRED_GLOBAL_STARTUP_SLOTS
        .iter()
        .chain(REQUIRED_COMPOSER_STARTUP_SLOTS)
        .chain(view_slots)
        .copied()
        .filter(|required| !slots.iter().any(|slot| slot == required))
        .collect()
}

fn remove_theme(websocket_url: &str) -> Result<bool, CodexError> {
    let mut socket = connect_theme_channel(websocket_url)?;
    let expression = r#"(() => {
      const runtime = window.__codexThemeRuntime;
      if (runtime?.restoreTheme) return runtime.restoreTheme(runtime.sessionId, false);
      runtime?.observer?.disconnect();
      runtime?.schemeObserver?.disconnect();
      runtime?.composerLayoutObserver?.disconnect();
      if (runtime?.colorSchemeMedia && runtime?.syncColorScheme) {
        runtime.colorSchemeMedia.removeEventListener('change', runtime.syncColorScheme);
      }
      const eventTarget = runtime?.eventTarget ?? runtime?.root;
      if (eventTarget && runtime?.handleInput) {
        eventTarget.removeEventListener('input', runtime.handleInput, true);
      }
      if (eventTarget && runtime?.handleNavigation) {
        eventTarget.removeEventListener('click', runtime.handleNavigation, true);
      }
      if (runtime?.handleResize) {
        window.removeEventListener('resize', runtime.handleResize);
      }
      if (runtime?.frame) cancelAnimationFrame(runtime.frame);
      if (runtime?.homeFrame) cancelAnimationFrame(runtime.homeFrame);
      if (runtime?.contentFrame) cancelAnimationFrame(runtime.contentFrame);
      if (runtime?.metricsFrame) cancelAnimationFrame(runtime.metricsFrame);
      if (runtime?.resizeFrame) cancelAnimationFrame(runtime.resizeFrame);
      delete window.__codexThemeRuntime;
      document.querySelectorAll('[data-ct-home-prompt-native]').forEach(node => {
        const display = node.getAttribute('data-ct-home-prompt-display');
        const priority = node.getAttribute('data-ct-home-prompt-display-priority') ?? '';
        if (display === null) node.style.removeProperty('display');
        else node.style.setProperty('display', display, priority);
        node.removeAttribute('data-ct-home-prompt-native');
        node.removeAttribute('data-ct-home-prompt-display');
        node.removeAttribute('data-ct-home-prompt-display-priority');
      });
      document.getElementById('codex-theme-runtime-style')?.remove();
      document.documentElement?.removeAttribute('data-ct-theme');
      document.documentElement?.removeAttribute('data-ct-view');
      document.documentElement?.removeAttribute('data-ct-color-scheme');
      document.documentElement?.style.removeProperty('--ct-home-card-count');
      document.documentElement?.style.removeProperty('--ct-titlebar-safe-top');
      document.querySelectorAll('[data-ct-slot="conversation.stage"]').forEach(node => {
        node.style.removeProperty('--ct-conversation-banner-clearance');
        node.style.removeProperty('--ct-conversation-summary-width');
        node.style.removeProperty('--ct-conversation-content-left');
        node.style.removeProperty('--ct-conversation-content-width');
        node.style.removeProperty('--ct-conversation-header-safe-top');
      });
      document.querySelectorAll('[data-ct-sidebar-footer-height]').forEach(node => {
        const original = node.getAttribute('data-ct-sidebar-footer-height');
        if (original) node.style.setProperty('--sidebar-footer-height', original);
        else node.style.removeProperty('--sidebar-footer-height');
        node.removeAttribute('data-ct-sidebar-footer-height');
      });
      document.querySelectorAll('[data-ct-mount]').forEach(node => node.remove());
      document.querySelectorAll('[data-ct-slot]').forEach(node => node.removeAttribute('data-ct-slot'));
      document.querySelectorAll('[data-ct-composer-layout]')
        .forEach(node => node.removeAttribute('data-ct-composer-layout'));
      document.querySelectorAll('[data-ct-native-titlebar]')
        .forEach(node => node.removeAttribute('data-ct-native-titlebar'));
      return !document.getElementById('codex-theme-runtime-style')
        && !document.documentElement?.hasAttribute('data-ct-theme')
        && !document.documentElement?.hasAttribute('data-ct-color-scheme')
        && !document.querySelector('[data-ct-slot]')
        && !document.querySelector('[data-ct-mount]');
    })()"#;
    let removed = evaluate(&mut socket, 1, expression);
    let restored_csp = set_page_csp_bypass(&mut socket, 2, false);
    let _ = socket.close(None);
    let removed = removed?;
    restored_csp?;
    Ok(removed)
}

fn renew_theme_lease(websocket_url: &str, session_id: u64) -> Result<bool, CodexError> {
    let mut socket = connect_theme_channel(websocket_url)?;
    let expression = renew_theme_lease_expression(session_id, PAGE_LEASE_DURATION);
    let renewed = evaluate(&mut socket, 1, &expression)?;
    let _ = socket.close(None);
    Ok(renewed)
}

fn renew_theme_lease_expression(session_id: u64, duration: Duration) -> String {
    let page_lease_milliseconds = duration.as_millis();
    format!(
        r#"(() => {{
          const runtime = window.__codexThemeRuntime;
          if (!runtime || runtime.sessionId !== {session_id}) return false;
          const now = Date.now();
          if (runtime.hardExpiresAt && now >= runtime.hardExpiresAt) {{
            runtime.restoreTheme?.(runtime.sessionId, true);
            return false;
          }}
          runtime.leaseExpiresAt = Math.min(
            now + {page_lease_milliseconds},
            runtime.hardExpiresAt ?? Number.POSITIVE_INFINITY
          );
          return true;
        }})()"#
    )
}

fn wait_for_codex_target(
    port: u16,
    process: &mut IsolatedProcess,
) -> Result<DevToolsTarget, CodexError> {
    let started = Instant::now();
    while started.elapsed() < STARTUP_TIMEOUT {
        if process.has_exited()? {
            return Err(CodexError("隔离 ChatGPT 提前退出".into()));
        }
        if let Ok(Some(target)) = find_codex_target(port, Duration::from_millis(500)) {
            return Ok(target);
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(CodexError("等待通过身份验证的 ChatGPT 页面目标超时".into()))
}

fn find_codex_target(port: u16, timeout: Duration) -> Result<Option<DevToolsTarget>, CodexError> {
    let targets = get_json_with_timeout::<Vec<DevToolsTarget>>(port, "/json/list", timeout)?;
    Ok(targets.into_iter().find(|target| {
        target.target_type == "page"
            && target.url == "app://-/index.html"
            && target
                .web_socket_debugger_url
                .starts_with(&format!("ws://127.0.0.1:{port}/devtools/page/"))
    }))
}

fn start_isolated(installation: &CodexInstallation) -> Result<TestInstance, CodexError> {
    let profile = tempfile::Builder::new()
        .prefix("codex-theme-smoke-")
        .tempdir()?;

    #[cfg(target_os = "windows")]
    {
        let arguments = isolated_launch_arguments(profile.path());
        let command_line = windows_command_line(&arguments);
        let process = activate_windows_store_app(&installation.app_user_model_id, &command_line)?;
        return Ok(TestInstance {
            process: IsolatedProcess::WindowsStore(process),
            _profile: profile,
        });
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut command = Command::new(&installation.executable);
        command
            .args(isolated_launch_arguments(profile.path()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(target_os = "macos")]
        command.process_group(0);
        let child = command
            .spawn()
            .map_err(|error| CodexError(format!("无法启动隔离 ChatGPT：{error}")))?;
        #[cfg(target_os = "macos")]
        let process_group_id = Some(child.id() as i32);
        Ok(TestInstance {
            process: IsolatedProcess::Native {
                child,
                #[cfg(target_os = "macos")]
                process_group_id,
            },
            _profile: profile,
        })
    }
}

fn isolated_launch_arguments(profile: &Path) -> Vec<String> {
    vec![
        format!("--user-data-dir={}", profile.display()),
        "--remote-debugging-address=127.0.0.1".into(),
        "--remote-debugging-port=0".into(),
        "--disable-background-timer-throttling".into(),
        "--no-first-run".into(),
    ]
}

#[cfg(target_os = "windows")]
fn windows_command_line(arguments: &[String]) -> String {
    arguments
        .iter()
        .map(|argument| quote_windows_argument(argument))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(target_os = "windows")]
fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| matches!(character, ' ' | '\t' | '"'))
    {
        return argument.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(target_os = "windows")]
fn activate_windows_store_app(
    app_user_model_id: &str,
    arguments: &str,
) -> Result<WindowsStoreProcess, CodexError> {
    use windows::Win32::System::Com::{
        CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        AO_NONE, ApplicationActivationManager, IApplicationActivationManager,
    };
    use windows::core::HSTRING;

    struct ComApartment(bool);

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let initialized_here = if initialized.is_ok() {
        true
    } else if initialized == windows::Win32::Foundation::RPC_E_CHANGED_MODE {
        false
    } else {
        return Err(CodexError(format!(
            "无法初始化 Windows 应用激活环境：{}",
            windows::core::Error::from_hresult(initialized)
        )));
    };
    let _apartment = ComApartment(initialized_here);
    let manager: IApplicationActivationManager =
        unsafe { CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|error| CodexError(format!("无法创建 Windows 应用激活管理器：{error}")))?;
    let app_user_model_id = HSTRING::from(app_user_model_id);
    let arguments = HSTRING::from(arguments);
    let process_id =
        unsafe { manager.ActivateApplication(&app_user_model_id, &arguments, AO_NONE) }.map_err(
            |error| {
                CodexError(format!(
                    "无法通过 Windows 应用入口启动隔离 ChatGPT：{error}"
                ))
            },
        )?;
    WindowsStoreProcess::open(process_id)
}

fn wait_for_devtools(
    profile: &Path,
    process: &mut IsolatedProcess,
) -> Result<(u16, String), CodexError> {
    let active_port_file = profile.join("DevToolsActivePort");
    let started = Instant::now();
    while started.elapsed() < STARTUP_TIMEOUT {
        if process.has_exited()? {
            return Err(CodexError("隔离 ChatGPT 提前退出".into()));
        }
        if let Ok(contents) = fs::read_to_string(&active_port_file) {
            let mut lines = contents.lines();
            let port = lines
                .next()
                .and_then(|value| value.parse::<u16>().ok())
                .ok_or_else(|| CodexError("DevToolsActivePort 端口无效".into()))?;
            let browser_path = lines
                .next()
                .filter(|value| value.starts_with("/devtools/browser/"))
                .ok_or_else(|| CodexError("DevToolsActivePort 目标路径无效".into()))?;
            return Ok((port, browser_path.to_owned()));
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(CodexError("等待 ChatGPT 本地主题通道超时".into()))
}

fn verify_loopback(socket: SocketAddr, _process_id: u32) -> Result<bool, CodexError> {
    if !socket.ip().is_loopback() {
        return Err(CodexError("本地主题通道地址不是回环地址".into()));
    }
    TcpStream::connect_timeout(&socket, Duration::from_secs(2))
        .map_err(|error| CodexError(format!("无法连接本地主题通道：{error}")))?;
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("/usr/sbin/lsof")
            .args([
                "-nP",
                "-a",
                "-p",
                &_process_id.to_string(),
                &format!("-iTCP:{}", socket.port()),
                "-sTCP:LISTEN",
                "-Fn",
            ])
            .output()
            .map_err(|error| CodexError(format!("无法检查本地主题通道监听地址：{error}")))?;
        if !output.status.success() {
            return Err(CodexError("无法确认本地主题通道实际监听地址".into()));
        }
        let endpoints = String::from_utf8(output.stdout)
            .map_err(|error| CodexError(format!("本地主题通道监听信息无效：{error}")))?;
        let expected = format!("n127.0.0.1:{}", socket.port());
        let addresses: Vec<&str> = endpoints
            .lines()
            .filter(|line| line.starts_with('n'))
            .collect();
        if addresses.is_empty() || addresses.iter().any(|address| *address != expected) {
            return Err(CodexError(format!(
                "本地主题通道未严格限定到 IPv4 回环地址：{}",
                addresses.join(", ")
            )));
        }
    }

    Ok(true)
}

fn get_json<T: for<'de> Deserialize<'de>>(port: u16, path: &str) -> Result<T, CodexError> {
    get_json_with_timeout(port, path, Duration::from_secs(3))
}

fn get_json_with_timeout<T: for<'de> Deserialize<'de>>(
    port: u16,
    path: &str,
    timeout: Duration,
) -> Result<T, CodexError> {
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
            .as_bytes(),
    )?;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(CodexError("本地主题通道响应提前结束".into()));
        }
        response.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") {
            let headers = std::str::from_utf8(&response[..header_end])
                .map_err(|error| CodexError(format!("本地主题通道响应头无效：{error}")))?;
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .ok_or_else(|| CodexError("本地主题通道响应缺少 Content-Length".into()))?;
            break (header_end + 4, content_length);
        }
    };
    while response.len() - header_end < content_length {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            return Err(CodexError("本地主题通道响应体提前结束".into()));
        }
        response.extend_from_slice(&buffer[..read]);
    }
    let headers = std::str::from_utf8(&response[..header_end - 4])
        .map_err(|error| CodexError(format!("本地主题通道响应头无效：{error}")))?;
    if !headers.starts_with("HTTP/1.1 200") {
        return Err(CodexError(format!("本地主题通道请求失败：{headers}")));
    }
    serde_json::from_slice(&response[header_end..header_end + content_length])
        .map_err(|error| CodexError(format!("本地主题通道数据无效：{error}")))
}

fn connect_theme_channel(websocket_url: &str) -> Result<WebSocket<TcpStream>, CodexError> {
    let uri = websocket_url
        .parse::<tungstenite::http::Uri>()
        .map_err(|error| CodexError(format!("本地主题通道地址无效：{error}")))?;
    if uri.scheme_str() != Some("ws") {
        return Err(CodexError("本地主题通道必须使用回环明文连接".into()));
    }
    let host = uri
        .host()
        .ok_or_else(|| CodexError("本地主题通道地址缺少主机".into()))?
        .trim_start_matches('[')
        .trim_end_matches(']');
    let ip = host
        .parse::<IpAddr>()
        .map_err(|_| CodexError("本地主题通道必须使用回环地址".into()))?;
    if !ip.is_loopback() {
        return Err(CodexError("本地主题通道必须使用回环地址".into()));
    }
    let address = SocketAddr::new(ip, uri.port_u16().unwrap_or(80));
    let stream = TcpStream::connect_timeout(&address, THEME_CHANNEL_IO_TIMEOUT)
        .map_err(|error| CodexError(format!("无法连接本地主题通道：{error}")))?;
    stream
        .set_read_timeout(Some(THEME_CHANNEL_IO_TIMEOUT))
        .map_err(|error| CodexError(format!("无法配置本地主题通道读取超时：{error}")))?;
    stream
        .set_write_timeout(Some(THEME_CHANNEL_IO_TIMEOUT))
        .map_err(|error| CodexError(format!("无法配置本地主题通道写入超时：{error}")))?;
    stream
        .set_nodelay(true)
        .map_err(|error| CodexError(format!("无法配置本地主题通道：{error}")))?;
    let (socket, _) = client(websocket_url, stream)
        .map_err(|error| CodexError(format!("无法握手本地主题通道：{error}")))?;
    Ok(socket)
}

fn test_injection(websocket_url: &str) -> Result<(bool, bool), CodexError> {
    let mut socket = connect_theme_channel(websocket_url)?;
    let apply_expression = r#"(() => {
      if (!document.documentElement) return false;
      const id = 'codex-theme-smoke-probe';
      document.getElementById(id)?.remove();
      const style = document.createElement('style');
      style.id = id;
      style.textContent = ':root { --ct-smoke-probe: 1; }';
      document.documentElement.appendChild(style);
      document.documentElement.setAttribute('data-ct-smoke', 'active');
      return getComputedStyle(document.documentElement).getPropertyValue('--ct-smoke-probe').trim() === '1';
    })()"#;
    let remove_expression = r#"(() => {
      document.getElementById('codex-theme-smoke-probe')?.remove();
      document.documentElement?.removeAttribute('data-ct-smoke');
      return !document.getElementById('codex-theme-smoke-probe') && !document.documentElement?.hasAttribute('data-ct-smoke');
    })()"#;

    let started = Instant::now();
    let mut command_id = 1;
    let applied = loop {
        if evaluate(&mut socket, command_id, apply_expression)? {
            break true;
        }
        if started.elapsed() >= STARTUP_TIMEOUT {
            break false;
        }
        command_id += 1;
        thread::sleep(Duration::from_millis(100));
    };
    let removed = evaluate(&mut socket, command_id + 1, remove_expression)?;
    let _ = socket.close(None);
    Ok((applied, removed))
}

fn evaluate<S>(
    socket: &mut tungstenite::WebSocket<S>,
    id: u64,
    expression: &str,
) -> Result<bool, CodexError>
where
    S: Read + Write,
{
    evaluate_value(socket, id, expression)?
        .as_bool()
        .ok_or_else(|| CodexError("本地主题通道没有返回布尔结果".into()))
}

fn set_page_csp_bypass<S>(
    socket: &mut tungstenite::WebSocket<S>,
    id: u64,
    enabled: bool,
) -> Result<(), CodexError>
where
    S: Read + Write,
{
    socket
        .send(Message::Text(
            json!({
                "id": id,
                "method": "Page.setBypassCSP",
                "params": { "enabled": enabled }
            })
            .to_string()
            .into(),
        ))
        .map_err(|error| CodexError(format!("设置本地主题资源通道失败：{error}")))?;
    loop {
        let message = socket
            .read()
            .map_err(|error| CodexError(format!("读取本地主题资源通道响应失败：{error}")))?;
        if !message.is_text() {
            continue;
        }
        let response: Value = serde_json::from_str(
            message
                .to_text()
                .map_err(|error| CodexError(format!("本地主题资源通道响应无效：{error}")))?,
        )
        .map_err(|error| CodexError(format!("本地主题资源通道数据无效：{error}")))?;
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(CodexError(format!("设置本地主题资源通道失败：{error}")));
        }
        return Ok(());
    }
}

fn evaluate_value<S>(
    socket: &mut tungstenite::WebSocket<S>,
    id: u64,
    expression: &str,
) -> Result<Value, CodexError>
where
    S: Read + Write,
{
    evaluate_value_until(
        socket,
        id,
        expression,
        Instant::now() + THEME_CHANNEL_IO_TIMEOUT,
    )
}

fn evaluate_value_until<S>(
    socket: &mut tungstenite::WebSocket<S>,
    id: u64,
    expression: &str,
    deadline: Instant,
) -> Result<Value, CodexError>
where
    S: Read + Write,
{
    socket
        .send(Message::Text(
            json!({
                "id": id,
                "method": "Runtime.evaluate",
                "params": {
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true
                }
            })
            .to_string()
            .into(),
        ))
        .map_err(|error| CodexError(format!("发送本地主题通道命令失败：{error}")))?;

    loop {
        let message = read_theme_channel_message_until(socket, deadline)
            .map_err(|error| CodexError(format!("读取本地主题通道响应失败：{error}")))?;
        if !message.is_text() {
            continue;
        }
        let response: Value = serde_json::from_str(
            message
                .to_text()
                .map_err(|error| CodexError(format!("本地主题通道文本响应无效：{error}")))?,
        )
        .map_err(|error| CodexError(format!("本地主题通道响应数据无效：{error}")))?;
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = response.get("error") {
            return Err(CodexError(format!("本地主题通道命令返回错误：{error}")));
        }
        if let Some(exception) = response.pointer("/result/exceptionDetails") {
            return Err(CodexError(format!("本地主题通道脚本执行失败：{exception}")));
        }
        return response
            .pointer("/result/result/value")
            .cloned()
            .ok_or_else(|| CodexError("本地主题通道命令没有返回值".into()));
    }
}

fn read_theme_channel_message_until<S>(
    socket: &mut tungstenite::WebSocket<S>,
    deadline: Instant,
) -> Result<Message, tungstenite::Error>
where
    S: Read + Write,
{
    loop {
        if Instant::now() >= deadline {
            return Err(tungstenite::Error::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "等待本地主题通道响应超时",
            )));
        }
        match socket.read() {
            Ok(message) => return Ok(message),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_theme_channel_response_across_temporary_read_timeouts() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("test listener");
        let address = listener.local_addr().expect("test listener address");
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("test connection");
            let mut socket = tungstenite::accept(stream).expect("test websocket handshake");
            let request = socket.read().expect("runtime evaluate request");
            let request: Value = serde_json::from_str(request.to_text().expect("text request"))
                .expect("runtime evaluate json");
            let id = request["id"].as_u64().expect("runtime evaluate id");
            thread::sleep(Duration::from_millis(120));
            socket
                .send(Message::Text(
                    json!({
                        "id": id,
                        "result": { "result": { "value": true } }
                    })
                    .to_string()
                    .into(),
                ))
                .expect("runtime evaluate response");
        });
        let stream = TcpStream::connect(address).expect("test client connection");
        stream
            .set_read_timeout(Some(Duration::from_millis(40)))
            .expect("test read timeout");
        let url = format!("ws://{address}/devtools/page/test");
        let (mut socket, _) = client(url, stream).expect("test websocket client");

        let response = evaluate_value_until(
            &mut socket,
            7,
            "true",
            Instant::now() + Duration::from_millis(500),
        )
        .expect("response after temporary timeouts");

        assert_eq!(response, Value::Bool(true));
        server.join().expect("test server");
    }

    #[cfg(target_os = "macos")]
    fn external_test_theme() -> PathBuf {
        std::env::var_os("RETHEME_TEST_THEME_DIR")
            .map(PathBuf::from)
            .expect("set RETHEME_TEST_THEME_DIR to run ignored ChatGPT App theme tests")
    }

    fn test_report(port: u16) -> ThemePreviewReport {
        ThemePreviewReport {
            theme_id: "studio.example.test-theme".into(),
            theme: theme::test_theme_summary(),
            source: ThemePreviewSource::Installed,
            expires_at: None,
            app_version: "test".into(),
            port,
            applied_slots: vec!["app.shell".into()],
            loopback_only: true,
        }
    }

    fn runtime_with_session(instance: TestInstance, port: u16) -> ThemeRuntime {
        let runtime = ThemeRuntime::default();
        *runtime.session.lock().expect("theme session") = Some(ThemeSession {
            id: 1,
            instance,
            _asset_server: None,
            port,
            deadline: None,
            target_missing_since: None,
            report: test_report(port),
        });
        runtime
    }

    fn exited_test_instance() -> TestInstance {
        let mut command = if cfg!(target_os = "windows") {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit", "0"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("test child should start");
        child.wait().expect("test child should exit");
        TestInstance {
            process: IsolatedProcess::Native {
                child,
                #[cfg(target_os = "macos")]
                process_group_id: None,
            },
            _profile: tempfile::tempdir().expect("test profile"),
        }
    }

    fn running_test_instance() -> TestInstance {
        let mut command = if cfg!(target_os = "windows") {
            let mut command = Command::new("cmd");
            command.args(["/C", "ping", "-n", "30", "127.0.0.1"]);
            command
        } else {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 30"]);
            command
        };
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("test child should start");
        TestInstance {
            process: IsolatedProcess::Native {
                child,
                #[cfg(target_os = "macos")]
                process_group_id: None,
            },
            _profile: tempfile::tempdir().expect("test profile"),
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn quotes_msix_activation_arguments_using_windows_command_line_rules() {
        assert_eq!(quote_windows_argument("--no-first-run"), "--no-first-run");
        assert_eq!(
            quote_windows_argument("--user-data-dir=C:\\Users\\Test User\\profile"),
            "\"--user-data-dir=C:\\Users\\Test User\\profile\""
        );
        assert_eq!(quote_windows_argument(""), "\"\"");
        assert_eq!(quote_windows_argument("a\\\\\"b"), "\"a\\\\\\\\\\\"b\"");
        assert_eq!(
            quote_windows_argument("C:\\path with space\\"),
            "\"C:\\path with space\\\\\""
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn builds_msix_activation_command_line_with_isolated_profile() {
        let arguments = isolated_launch_arguments(Path::new("C:\\Temp\\ReTheme Profile"));
        let command_line = windows_command_line(&arguments);

        assert!(command_line.starts_with("\"--user-data-dir=C:\\Temp\\ReTheme Profile\" "));
        assert!(command_line.contains("--remote-debugging-address=127.0.0.1"));
        assert!(command_line.contains("--remote-debugging-port=0"));
        assert!(command_line.ends_with("--no-first-run"));
    }

    fn serve_devtools_target_responses(responses: Vec<&'static str>) -> u16 {
        let listener =
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("test devtools listener");
        let port = listener.local_addr().expect("test listener address").port();
        thread::spawn(move || {
            for response in responses {
                let response = response.replace("{port}", &port.to_string());
                let (mut stream, _) = listener.accept().expect("test devtools connection");
                let mut request = [0_u8; 1024];
                let _bytes_read = stream.read(&mut request).expect("test devtools request");
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                            response.len()
                        )
                        .as_bytes(),
                    )
                    .expect("test devtools response");
            }
        });
        port
    }

    fn runtime_with_exited_session() -> ThemeRuntime {
        runtime_with_session(exited_test_instance(), 18926)
    }

    #[test]
    fn clears_preview_status_after_codex_exits() {
        let runtime = runtime_with_exited_session();

        assert!(runtime.current_preview().expect("preview status").is_none());
        assert!(runtime.session.lock().expect("theme session").is_none());
    }

    #[test]
    fn keeps_preview_during_brief_target_loss() {
        let port = serve_devtools_target_responses(vec!["[]", "[]", "[]"]);
        let runtime = runtime_with_session(running_test_instance(), port);

        assert!(
            runtime
                .current_preview()
                .expect("first preview status")
                .is_some()
        );
        assert!(
            runtime
                .current_preview()
                .expect("second preview status")
                .is_some()
        );
        assert!(
            runtime
                .current_preview()
                .expect("third preview status")
                .is_some()
        );
        assert!(runtime.session.lock().expect("theme session").is_some());
    }

    #[test]
    fn keeps_preview_during_transient_page_transition() {
        let page = r#"[{"title":"Codex","type":"page","url":"app://-/index.html","webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/page/test"}]"#;
        let port = serve_devtools_target_responses(vec!["[]", page]);
        let runtime = runtime_with_session(running_test_instance(), port);

        assert!(
            runtime
                .current_preview()
                .expect("transition status")
                .is_some()
        );
        assert!(
            runtime
                .current_preview()
                .expect("restored status")
                .is_some()
        );
        assert_eq!(
            runtime
                .session
                .lock()
                .expect("theme session")
                .as_ref()
                .expect("active session")
                .target_missing_since,
            None
        );
    }

    #[test]
    fn page_lease_renewal_is_bound_to_session_and_hard_deadline() {
        let expression = renew_theme_lease_expression(42, Duration::from_secs(15));

        assert!(expression.contains("runtime.sessionId !== 42"));
        assert!(expression.contains("now + 15000"));
        assert!(expression.contains("runtime.hardExpiresAt && now >= runtime.hardExpiresAt"));
        assert!(expression.contains("Math.min("));
        assert!(expression.contains("runtime.restoreTheme?.(runtime.sessionId, true)"));
    }

    #[test]
    fn page_lease_timing_keeps_recovery_margin() {
        assert!(PAGE_LEASE_RENEW_INTERVAL < PAGE_LEASE_DURATION);
        assert!(PAGE_LEASE_RENEW_INTERVAL + THEME_CHANNEL_IO_TIMEOUT < PAGE_LEASE_DURATION);
    }

    #[test]
    fn repeated_apply_keeps_current_session_assets_alive() {
        let source = include_str!("codex.rs");

        assert!(source.contains("existingRuntime?.sessionId === sessionId"));
        assert!(source.contains("const refreshedSlots = existingRuntime.apply()"));
        assert!(source.contains("existingRuntime.leaseExpiresAt = Math.min("));
        assert!(source.contains("return existingRuntime.slots ?? []"));
        assert!(source.contains("runtime.slots = apply() ?? runtime.slots"));
        assert!(source.contains("if (revokeAssets) runtime?.revokeAssets?.()"));
        assert!(source.contains("existingRuntime?.revokeAssets?.()"));
        assert!(source.contains("runtime.restoreTheme(runtime.sessionId, false)"));
    }

    #[test]
    fn theme_locale_is_owned_by_retheme_and_can_sync_live() {
        let source = include_str!("codex.rs");

        assert_eq!(normalize_theme_locale("zh-CN"), "zh-CN");
        assert_eq!(normalize_theme_locale("zh_TW"), "zh-CN");
        assert_eq!(normalize_theme_locale("en-US"), "en");
        assert!(source.contains("let requestedLocale = {locale};"));
        assert!(source.contains("runtime.syncLocale = locale =>"));
        let browser_locale_fallback =
            ["document.documentElement?.lang", " || navigator.language"].concat();
        assert!(!source.contains(&browser_locale_fallback));
    }

    #[test]
    fn app_background_is_a_non_interactive_bottom_layer() {
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"app.shell\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("isolation: isolate !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-mount=\"app.background\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("z-index: -1 !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("pointer-events: none !important"));
    }

    #[test]
    fn controlled_assets_switch_with_chatgpt_appearance() {
        let source = include_str!("codex.rs");

        assert!(source.contains("const assetUrlForScheme = asset =>"));
        assert!(source.contains("asset.lightAssetUrl"));
        assert!(source.contains("asset.darkAssetUrl"));
        assert!(source.contains("syncManagedAssetSchemes();"));
        assert!(source.contains("const assetUrl = assetUrlForScheme(asset);"));
    }

    #[test]
    fn card_background_is_a_non_interactive_inner_layer() {
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"home.card\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-mount=\"home.card.background\"]",));
        assert!(PLATFORM_RUNTIME_CSS.contains("border-radius: inherit !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("pointer-events: none !important"));
    }

    #[test]
    fn compact_home_hides_full_home_content() {
        let source = include_str!("codex.rs");

        assert!(
            PLATFORM_RUNTIME_CSS
                .contains(":root[data-ct-view=\"home-compact\"] [data-ct-slot=\"home.prompt\"]")
        );
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains(":root[data-ct-view=\"home-compact\"] [data-ct-slot=\"home.cards\"]")
        );
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains(":root[data-ct-view=\"home-compact\"] [data-ct-slot=\"home.layout\"]")
        );
        assert!(
            PLATFORM_RUNTIME_CSS.contains(
                ":root[data-ct-view=\"home-compact\"] [data-ct-slot=\"composer.region\"]"
            )
        );
        assert!(PLATFORM_RUNTIME_CSS.contains("min-height: 100% !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("max-width: none !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("align-self: stretch !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("margin-right: auto !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("margin-left: auto !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("justify-content: flex-end !important"));
        assert!(source.contains("mountConversationBanner(main, editor, slots, homeLayout)"));
    }

    #[test]
    fn banner_foregrounds_are_non_interactive_bottom_aligned_layers() {
        let source = include_str!("codex.rs");

        for slot in ["home.hero.foreground", "conversation.banner.foreground"] {
            assert!(
                PLATFORM_RUNTIME_CSS.contains(&format!("[data-ct-slot=\"{slot}\"]")),
                "missing platform layout for {slot}"
            );
        }
        assert!(PLATFORM_RUNTIME_CSS.contains("--ct-home-hero-foreground-bottom"));
        assert!(PLATFORM_RUNTIME_CSS.contains("--ct-conversation-banner-foreground-bottom"));
        assert!(PLATFORM_RUNTIME_CSS.contains("object-position: center bottom !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("pointer-events: none !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains(
            "max-height: var(--ct-conversation-banner-foreground-safe-height, none) !important"
        ));
        assert!(source.contains("const syncConversationBannerForeground = () =>"));
        assert!(source.contains("ancestor.getBoundingClientRect().top + ancestor.clientTop"));
        assert!(source.contains("syncConversationBannerForeground();"));
    }

    #[test]
    fn conversation_header_keeps_banner_outside_the_message_scroller() {
        let source = include_str!("codex.rs");

        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"conversation.stage\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"conversation.header\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"conversation.header.content\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"conversation.viewport\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("flex-direction: column !important"));
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains("padding-top: var(--ct-conversation-header-safe-top, 0px) !important")
        );
        assert!(!PLATFORM_RUNTIME_CSS.contains(
            "grid-template-columns:\n    minmax(0, 1fr)\n    var(--ct-conversation-summary-width"
        ));
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains("width: var(--ct-conversation-content-width, 100%) !important")
        );
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains("margin-left: var(--ct-conversation-content-left, 0px) !important")
        );
        assert!(PLATFORM_RUNTIME_CSS.contains("border: 0 !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("box-shadow: none !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains(
            ":where([data-ct-slot=\"conversation.stage\"], [data-ct-slot=\"conversation.viewport\"])::before"
        ));
        assert!(source.contains("stage.insertBefore(header, viewport)"));
        assert!(source.contains("content.appendChild(banner)"));
        assert!(source.contains("const syncConversationHeaderLayout = () =>"));
        assert!(source.contains("composerRect.left - stageRect.left"));
        assert!(source.contains("stageRect.right - composerRect.right"));
        assert!(source.contains("titlebarBottom - stageRect.top"));
        assert!(source.contains("syncConversationHeaderLayout();"));
        assert!(source.contains("markConversationSummary(stage, slots)"));
        assert!(source.contains("config.conversationSummaryDecoration?.assetUrl"));
        assert!(source.contains("'conversation.summary.decoration'"));
        assert!(!PLATFORM_RUNTIME_CSS.contains("--ct-conversation-banner-clearance"));
        assert!(source.contains("conversation.summary.region"));
    }

    #[test]
    fn conversation_header_metrics_are_reversible_and_resize_only_updates_geometry() {
        let source = include_str!("codex.rs");
        let runtime_source = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("runtime source");

        for property in [
            "--ct-conversation-content-left",
            "--ct-conversation-content-width",
            "--ct-conversation-header-safe-top",
        ] {
            assert!(
                runtime_source
                    .matches(&format!("removeProperty('{property}')"))
                    .count()
                    >= 3,
                "missing cleanup for {property}"
            );
        }
        assert!(runtime_source.contains("runtime.handleResize = () =>"));
        assert!(runtime_source.contains("runtime.resizeFrame = requestAnimationFrame"));
        assert!(!runtime_source.contains("runtime.handleResize = scheduleApply"));
    }

    #[test]
    fn every_visible_timeline_uses_the_conversation_layout() {
        let source = include_str!("codex.rs");

        assert!(source.contains(
            "const conversation = [...document.querySelectorAll(\n              adapter.selectors.conversation\n            )].find(isVisible);"
        ));
        assert!(source.contains("const isConversation = Boolean(conversation);"));
        assert!(source.contains("isConversation ? 'conversation' : 'other'"));
        assert!(source.contains("isConversation ? conversation : null"));
    }

    #[test]
    fn conversation_composer_backdrop_is_removed_without_hiding_composer() {
        let source = include_str!("codex.rs");

        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"composer.backdrop\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("display: none !important"));
        assert!(source.contains("getComputedStyle(composerSticky).position === 'sticky'"));
        assert!(source.contains("style.pointerEvents === 'none'"));
        assert!(source.contains("backgroundImage.includes('gradient')"));
    }

    #[test]
    fn composer_editor_tracks_native_compact_and_expanded_layouts() {
        let source = include_str!("codex.rs");

        assert!(PLATFORM_RUNTIME_CSS.contains(
            "[data-ct-slot=\"composer.editor\"][data-ct-composer-layout=\"compact\"][contenteditable=\"true\"]"
        ));
        assert!(PLATFORM_RUNTIME_CSS.contains("min-height: 1.25rem !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("padding-block: 0 !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("line-height: 1.25rem !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("margin-block: 0 !important"));
        assert!(source.contains("const syncComposerLayout = editor =>"));
        assert!(source.contains("frameHeight > 0 && frameHeight <= 44"));
        assert!(source.contains("? 'compact'\n              : 'expanded'"));
        assert!(source.contains("const composerLayoutObserver = new ResizeObserver"));
        assert!(source.contains("composerLayoutObserver.observe(target)"));
        assert!(source.contains("runtime?.composerLayoutObserver?.disconnect()"));
    }

    #[test]
    fn main_content_frame_has_no_native_separator() {
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"main.content.frame\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("border: 0 !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("box-shadow: none !important"));
    }

    #[test]
    fn home_layout_separates_stage_from_composer_region() {
        let source = include_str!("codex.rs");

        assert!(source.contains("let homeLayout = homeSource"));
        assert!(source.contains("const homeBranch = directLayoutBranch(homeSource)"));
        assert!(source.contains("const composerRegion = directLayoutBranch(composerRoot)"));
        assert!(source.contains("homeLayout.setAttribute('data-ct-slot', 'home.layout')"));
        assert!(source.contains("composerRegion.setAttribute('data-ct-slot', 'composer.region')"));
    }

    #[test]
    fn composer_slots_follow_the_visible_editor_across_routes() {
        let source = include_str!("codex.rs");

        assert!(source.contains(
            "const editors = [...document.querySelectorAll(adapter.selectors.composer)]"
        ));
        assert!(source.contains("const editor = editors.find(isVisible) ?? editors[0] ?? null"));
        assert!(source.contains("'[data-ct-slot=\"composer\"], [data-ct-slot^=\"composer.\"]'"));
    }

    #[test]
    fn composer_context_uses_the_remote_utility_bar_selector() {
        let source = include_str!("codex.rs");

        assert!(source.contains("adapter.selectors.composerUtilityBar"));
        assert!(source.contains("const utilityBarSurface = utilityBar?.parentElement ?? null"));
        assert!(source.contains("contextSlot = utilityBarSurface"));
        assert!(source.contains("!contextSlot?.contains(button)"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"composer.context\"]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("backdrop-filter: none !important"));
    }

    #[test]
    fn runtime_survives_settings_and_root_replacement() {
        let source = include_str!("codex.rs");

        assert!(source.contains("let runtimeInstalled = false"));
        assert!(source.contains("if (!runtimeInstalled\n              && !adapter.probes.every"));
        assert!(source.contains(
            "const appMain = appMainCandidates.find(isVisible) ?? appMainCandidates[0] ?? null"
        ));
        assert!(source.contains("eventTarget: document"));
        assert!(
            source.contains("runtime.observer.observe(document.documentElement, observeOptions)")
        );
        assert!(source.contains("runtime.root = document.getElementById('root')"));
        assert!(source.contains("runtime.lastApplyError ="));
    }

    #[test]
    fn conversation_content_and_resize_do_not_remount_layout() {
        let source = include_str!("codex.rs");
        let runtime_source = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("runtime source");

        assert!(runtime_source.contains("const scheduleConversationContent = roots =>"));
        assert!(runtime_source.contains("const scheduleMetrics = () =>"));
        assert!(runtime_source.contains("runtime.handleResize = () =>"));
        assert!(runtime_source.contains("runtime.resizeFrame = requestAnimationFrame"));
        assert!(
            runtime_source.contains("if (conversationRoots.length) scheduleConversationContent")
        );
        assert!(!runtime_source.contains("runtime.handleResize = scheduleApply"));
        assert!(!runtime_source.contains("const scheduleResponsiveLayout = () =>"));
        assert!(!runtime_source.contains(
            "return routeBoundaryChanged || relevantNodeChanged || Boolean(target?.closest"
        ));
    }

    #[test]
    fn home_input_refreshes_only_home_slots() {
        let source = include_str!("codex.rs");
        let runtime_source = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("runtime source");

        assert!(runtime_source.contains("const scheduleHomeRefresh = () =>"));
        assert!(runtime_source.contains("runtime.handleInput = event =>"));
        assert!(runtime_source.contains("mountHero(appMain, editor, homeSlots)"));
        assert!(runtime_source.contains("syncComposerLayout(editor)"));
        assert!(runtime_source.contains("scheduleHomeRefresh();"));
        assert!(
            !runtime_source.contains(
                "composerRoot?.contains(event.target)) {{\n              scheduleApply();"
            )
        );
        assert_eq!(
            runtime_source
                .matches("if (runtime?.homeFrame) cancelAnimationFrame(runtime.homeFrame)")
                .count(),
            2
        );
        assert!(runtime_source.contains(
            "if (existingRuntime?.homeFrame) cancelAnimationFrame(existingRuntime.homeFrame)"
        ));
    }

    #[test]
    fn legacy_runtime_fallback_releases_every_observer_and_frame() {
        let source = include_str!("codex.rs");
        let fallback = source
            .split("else {{\n            existingRuntime?.observer?.disconnect();")
            .nth(1)
            .and_then(|source| {
                source
                    .split("document.querySelectorAll('[data-ct-managed-asset]')")
                    .next()
            })
            .expect("legacy runtime fallback");

        assert!(fallback.contains("existingRuntime?.composerLayoutObserver?.disconnect()"));
        for frame in [
            "frame",
            "homeFrame",
            "contentFrame",
            "metricsFrame",
            "resizeFrame",
        ] {
            assert!(
                fallback.contains(&format!(
                    "if (existingRuntime?.{frame}) cancelAnimationFrame(existingRuntime.{frame})"
                )),
                "missing cleanup for {frame}"
            );
        }
    }

    #[test]
    fn home_hero_uses_content_flow_and_workspace_panels_use_compact_home() {
        let source = include_str!("codex.rs");
        let runtime_source = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("runtime source");

        assert!(runtime_source.contains("homeStage.prepend(hero)"));
        assert!(runtime_source.contains("viewport.append(copy, media, divider)"));
        assert!(runtime_source.contains("hero.prepend(heroViewport)"));
        assert!(!runtime_source.contains("main.prepend(hero)"));
        assert!(runtime_source.contains("const hasWorkspacePanel = Boolean"));
        assert!(runtime_source.contains("editorHasDraft(editor) || hasWorkspacePanel"));
        assert!(runtime_source.contains("if (!hasWorkspacePanel)"));
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains(":root[data-ct-view=\"home\"] [data-ct-mount=\"home.hero\"]")
        );
        assert!(
            PLATFORM_RUNTIME_CSS.contains(
                "[data-ct-slot=\"home.hero.viewport\"] {\n  position: relative !important"
            )
        );
        assert!(
            !PLATFORM_RUNTIME_CSS.contains(
                "[data-ct-slot=\"home.hero.viewport\"] {\n  position: absolute !important"
            )
        );
        assert!(PLATFORM_RUNTIME_CSS.contains("width: 100% !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-slot=\"home.hero.viewport\"]"));
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains("overflow: var(--ct-home-hero-viewport-overflow, hidden) !important")
        );
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains("overflow: var(--ct-home-hero-media-overflow, hidden) !important")
        );
        assert!(
            !PLATFORM_RUNTIME_CSS
                .contains(":root[data-ct-view=\"home\"] [data-ct-slot=\"home.content.region\"]")
        );
        assert!(
            PLATFORM_RUNTIME_CSS
                .contains("max-width: var(--ct-home-hero-max-width, 1080px) !important")
        );
        assert!(PLATFORM_RUNTIME_CSS.contains("transform: none !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("[data-ct-workspace-panel-region]"));
    }

    #[test]
    fn windows_shell_surfaces_and_retheme_brand_are_injected_by_the_engine() {
        let source = include_str!("codex.rs");
        let runtime_source = source
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("runtime source");

        assert!(runtime_source.contains("const WINDOWS_PLATFORM_RUNTIME_CSS"));
        assert!(runtime_source.contains("[data-ct-native-titlebar]"));
        assert!(runtime_source.contains("markSlot(applicationMenus, 'titlebar', slots)"));
        assert!(!runtime_source.contains("'titlebar.menu',\n                slots"));
        let deprecated_titlebar_slot = ["titlebar", ".menu"].concat();
        assert!(!runtime_source.contains(&deprecated_titlebar_slot));
        assert!(
            runtime_source.contains(":root[data-ct-view=\"home\"] [data-ct-slot=\"home.stage\"]")
        );
        assert!(runtime_source.contains("justify-content: flex-start !important"));
        assert!(runtime_source.contains("padding-top: 0 !important"));
        assert!(runtime_source.contains("[data-ct-slot=\"settings.content\"]"));
        assert!(runtime_source.contains("[data-ct-slot=\"settings.frame\"]"));
        assert!(runtime_source.contains("[data-ct-slot=\"settings.toolbar\"]"));
        assert!(runtime_source.contains("margin-top: 0 !important"));
        assert!(runtime_source.contains("display: none !important"));
        assert!(runtime_source.contains("border-radius: 0 !important"));
        assert!(runtime_source.contains("label.textContent = 'ReTheme'"));
        assert!(!runtime_source.contains("label.textContent = 'reclaude'"));
    }

    #[test]
    fn card_label_targets_the_text_leaf_and_footer_brand_has_vertical_space() {
        let source = include_str!("codex.rs");

        assert!(source.contains("let label = labelContainer"));
        assert!(source.contains("if (textChildren.length !== 1) break"));
        assert!(source.contains("label = textChildren[0]"));
        assert!(PLATFORM_RUNTIME_CSS.contains("top: 0 !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("height: 32px !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("align-self: stretch !important"));
        assert!(!PLATFORM_RUNTIME_CSS.contains("top: 4px !important"));
        assert!(source.contains("version.textContent = `v${{themeVersion}}`"));
        assert!(source.contains("if (hasPro) slots.push('sidebar.footer.brand.pro')"));
        assert!(source.contains("'--sidebar-footer-height', '78px'"));
    }

    #[test]
    fn neutralizes_native_shell_overlays_without_removing_theme_borders() {
        assert!(PLATFORM_RUNTIME_CSS.contains(":where([data-ct-slot=\"titlebar\"])"));
        assert!(PLATFORM_RUNTIME_CSS.contains("background: transparent !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains("backdrop-filter: none !important"));
        assert!(PLATFORM_RUNTIME_CSS.contains(":where([data-ct-slot=\"sidebar\"])::after"));
        assert!(
            PLATFORM_RUNTIME_CSS.contains(":where([data-ct-slot=\"settings.sidebar\"])::after")
        );
        assert!(
            PLATFORM_RUNTIME_CSS.contains(":where([data-ct-slot=\"sidebar.resize.indicator\"])",)
        );
        assert!(PLATFORM_RUNTIME_CSS.contains(":where([data-ct-slot=\"main\"])",));
        assert!(PLATFORM_RUNTIME_CSS.contains(":where([data-ct-slot=\"main.fade\"])",));
        assert!(
            !PLATFORM_RUNTIME_CSS
                .contains(":where([data-ct-slot=\"sidebar\"]) {\n  border-right:",)
        );
        assert!(
            !PLATFORM_RUNTIME_CSS
                .contains(":where([data-ct-slot=\"settings.sidebar\"]) {\n  border-right:",)
        );
    }

    #[test]
    #[ignore = "launches an isolated themed ChatGPT App window and waits for its page lease"]
    #[cfg(target_os = "macos")]
    fn page_lease_restores_theme_without_runtime_heartbeat() {
        let installation = detect().expect("ChatGPT App installation");
        let mut instance = start_isolated(&installation).expect("isolated ChatGPT App");
        let (port, _) =
            wait_for_devtools(instance._profile.path(), &mut instance.process).expect("devtools");
        let target = wait_for_codex_target(port, &mut instance.process).expect("ChatGPT page");
        let (mut socket, _) = connect(&target.web_socket_debugger_url).expect("theme socket");
        let expression = format!(
            r#"(() => {{
              const root = document.documentElement;
              if (!root || !document.head || !document.body || document.readyState !== 'complete') {{
                return false;
              }}
              const runtimeKey = '__codexThemeRuntime';
              const style = document.createElement('style');
              style.id = 'codex-theme-runtime-style';
              root.appendChild(style);
              root.setAttribute('data-ct-theme', 'lease-test');
              const mount = document.createElement('div');
              mount.dataset.ctMount = 'lease-test';
              mount.dataset.ctSlot = 'lease-test';
              root.appendChild(mount);
              const runtime = {{
                sessionId: 77,
                hardExpiresAt: null,
                leaseExpiresAt: 0,
                leaseTimer: 0,
                restoreTheme: {restore_theme},
                observer: null,
                schemeObserver: null,
                colorSchemeMedia: null,
                syncColorScheme: null,
                root: null,
                handleInput: null,
                frame: 0
              }};
              window[runtimeKey] = runtime;
              ({lease_controller})(runtimeKey, runtime, 2000);
              return true;
            }})()"#,
            restore_theme = PAGE_RESTORE_THEME_FUNCTION,
            lease_controller = PAGE_LEASE_CONTROLLER_FUNCTION,
        );
        let started = Instant::now();
        let mut command_id = 1;
        loop {
            if evaluate(&mut socket, command_id, &expression).expect("install page lease") {
                break;
            }
            assert!(
                started.elapsed() < STARTUP_TIMEOUT,
                "document should become ready"
            );
            command_id += 1;
            thread::sleep(Duration::from_millis(100));
        }
        let _ = socket.close(None);
        thread::sleep(Duration::from_secs(4));

        let target = find_codex_target(port, Duration::from_secs(2)).expect("page target status");
        if let Some(target) = target {
            let (mut socket, _) = connect(&target.web_socket_debugger_url).expect("theme socket");
            let restored = evaluate(
                &mut socket,
                1,
                r#"!window.__codexThemeRuntime
                  && !document.getElementById('codex-theme-runtime-style')
                  && !document.documentElement.hasAttribute('data-ct-theme')
                  && !document.querySelector('[data-ct-slot]')
                  && !document.querySelector('[data-ct-mount]')"#,
            )
            .expect("restored theme state");
            let _ = socket.close(None);
            assert!(restored, "page lease should restore the official interface");
        }
        drop(instance);
    }

    #[test]
    fn rejects_expired_absolute_preview_deadline() {
        assert!(remaining_until(unix_time()).is_err());
    }

    #[test]
    fn serves_theme_assets_only_from_the_session_url() {
        let asset = theme::ThemeRuntimeAsset {
            path: "assets/test.svg".into(),
            mime: "image/svg+xml".into(),
            source: Arc::from(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>".to_vec()),
        };
        let (server, urls, revoke_url) =
            ThemeAssetServer::start(vec![asset]).expect("asset server");
        let url = &urls["assets/test.svg"];
        assert!(url.starts_with("http://127.0.0.1:"));
        assert!(!url.contains("assets/test.svg"));

        let response = get_theme_asset_response(url, None);
        assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
        let cache_header = b"Cache-Control: private, no-store\r\n";
        assert!(
            response
                .windows(cache_header.len())
                .any(|window| window == cache_header)
        );
        assert!(response.ends_with(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>"));

        let address = url_socket(url);
        let path = url
            .split_once(address.to_string().as_str())
            .expect("asset URL")
            .1;
        let mut delayed_stream = TcpStream::connect(address).expect("delayed asset connection");
        thread::sleep(Duration::from_millis(30));
        write!(
            delayed_stream,
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        )
        .expect("delayed asset request");
        let mut delayed_response = Vec::new();
        delayed_stream
            .read_to_end(&mut delayed_response)
            .expect("delayed asset response");
        assert!(delayed_response.starts_with(b"HTTP/1.1 200 OK\r\n"));

        let response = get_theme_asset_response(url, Some("localhost"));
        assert!(response.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
        let response = get_theme_asset_response(&revoke_url, None);
        assert!(response.starts_with(b"HTTP/1.1 204 No Content\r\n"));
        assert!(TcpStream::connect_timeout(&url_socket(url), Duration::from_millis(100)).is_err());
        drop(server);
        assert!(TcpStream::connect_timeout(&url_socket(url), Duration::from_millis(100)).is_err());
    }

    #[test]
    #[ignore = "launches an isolated ChatGPT App window"]
    #[cfg(target_os = "macos")]
    fn chatgpt_app_loads_session_asset_urls() {
        let installation = detect().expect("ChatGPT App installation");
        let mut instance = start_isolated(&installation).expect("isolated ChatGPT App");
        let (port, _) =
            wait_for_devtools(instance._profile.path(), &mut instance.process).expect("devtools");
        wait_for_codex_target(port, &mut instance.process).expect("ChatGPT page");
        let asset = theme::ThemeRuntimeAsset {
            path: "assets/test.svg".into(),
            mime: "image/svg+xml".into(),
            source: Arc::from(
                b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"2\" height=\"2\"/>".to_vec(),
            ),
        };
        let (_server, urls, _) = ThemeAssetServer::start(vec![asset]).expect("asset server");
        let asset_url = serde_json::to_string(&urls["assets/test.svg"]).expect("asset URL JSON");
        let expression = format!(
            r#"new Promise(resolve => {{
              const image = new Image();
              image.onload = () => resolve(true);
              image.onerror = () => resolve(false);
              image.src = {asset_url};
            }})"#
        );
        let started = Instant::now();
        loop {
            let loaded = find_codex_target(port, Duration::from_secs(1))
                .ok()
                .flatten()
                .and_then(|target| connect(&target.web_socket_debugger_url).ok())
                .and_then(|(mut socket, _)| {
                    set_page_csp_bypass(&mut socket, 1, true).ok()?;
                    let loaded = evaluate(&mut socket, 2, &expression).ok();
                    let _ = socket.close(None);
                    loaded
                })
                .unwrap_or(false);
            if loaded {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "ChatGPT App should load the session asset URL"
            );
            thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    #[ignore = "launches an isolated ChatGPT App window with a complete theme"]
    #[cfg(target_os = "macos")]
    fn external_theme_loads_all_session_assets() {
        let runtime = ThemeRuntime::default();
        let themes = theme::ThemeRepository::new(
            tempfile::tempdir()
                .expect("temporary directory")
                .path()
                .to_path_buf(),
        )
        .expect("theme repository");
        let compatibility_directory = tempfile::tempdir().expect("compatibility directory");
        let compatibility = compatibility::CompatibilityRepository::new(
            compatibility_directory.path().to_path_buf(),
        )
        .expect("compatibility repository");
        let theme_path = external_test_theme();
        start_development_theme_preview(
            &runtime,
            &themes,
            &compatibility,
            &theme_path,
            None,
            false,
            "zh-CN",
        )
        .expect("external theme preview");
        assert!(runtime.renew_page_lease().expect("renew asset test lease"));
        {
            let session = runtime.session.lock().expect("theme session");
            let asset_server = session
                .as_ref()
                .and_then(|session| session._asset_server.as_ref())
                .expect("active theme asset server");
            assert!(
                !asset_server.shutdown.load(Ordering::Acquire),
                "theme asset server was revoked while applying the same session"
            );
            assert!(
                TcpStream::connect_timeout(&asset_server.address, Duration::from_millis(100))
                    .is_ok(),
                "theme asset server stopped listening during apply"
            );
        }
        let websocket_url = {
            let mut session = runtime.session.lock().expect("theme session");
            let session = session.as_mut().expect("active theme session");
            wait_for_codex_target(session.port, &mut session.instance.process)
                .expect("ChatGPT App page")
                .web_socket_debugger_url
        };
        let (mut socket, _) = connect(&websocket_url).expect("theme socket");
        let report = evaluate_value(
            &mut socket,
            1,
            r#"new Promise(resolve => {
              const images = [...document.images].filter(image =>
                image.src.startsWith('http://127.0.0.1:')
              );
              const finish = () => resolve({
                count: images.length,
                failed: images
                  .filter(image => !image.complete || image.naturalWidth === 0)
                  .map(image => image.src)
              });
              if (images.every(image => image.complete)) return finish();
              let pending = images.filter(image => !image.complete).length;
              const settled = () => {
                pending -= 1;
                if (pending === 0) finish();
              };
              images.filter(image => !image.complete).forEach(image => {
                image.addEventListener('load', settled, { once: true });
                image.addEventListener('error', settled, { once: true });
              });
              setTimeout(finish, 3000);
            })"#,
        )
        .expect("theme asset report");
        let _ = socket.close(None);
        assert!(
            report["count"].as_u64().is_some_and(|count| count >= 6),
            "expected mounted external theme assets: {report}"
        );
        assert_eq!(
            report["failed"].as_array().map(Vec::len),
            Some(0),
            "{report}"
        );
        assert!(stop_theme_preview(&runtime).expect("stop external theme preview"));
    }

    fn get_theme_asset_response(url: &str, host: Option<&str>) -> Vec<u8> {
        let address = url_socket(url);
        let path = url
            .split_once(address.to_string().as_str())
            .expect("asset URL")
            .1;
        let mut stream =
            TcpStream::connect_timeout(&address, Duration::from_secs(1)).expect("asset connection");
        let host = host
            .map(str::to_owned)
            .unwrap_or_else(|| address.to_string());
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            host
        )
        .expect("asset request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("asset response");
        response
    }

    fn url_socket(url: &str) -> SocketAddr {
        url.strip_prefix("http://")
            .and_then(|url| url.split_once('/'))
            .and_then(|(address, _)| address.parse().ok())
            .expect("asset address")
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn dropping_test_instance_only_terminates_its_process_group() {
        let mut unrelated = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("unrelated test process");
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30 & wait"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().expect("grouped test process");
        let process_group_id = child.id() as i32;
        let instance = TestInstance {
            process: IsolatedProcess::Native {
                child,
                process_group_id: Some(process_group_id),
            },
            _profile: tempfile::tempdir().expect("test profile"),
        };

        assert!(process_group_exists(process_group_id));
        drop(instance);

        assert!(!process_group_exists(process_group_id));
        assert!(unrelated.try_wait().expect("unrelated status").is_none());
        unrelated.kill().expect("stop unrelated process");
        unrelated.wait().expect("reap unrelated process");
    }

    #[test]
    fn startup_requires_core_slots_but_not_optional_decorations_or_health_checks() {
        let slots = REQUIRED_GLOBAL_STARTUP_SLOTS
            .iter()
            .chain(REQUIRED_COMPOSER_STARTUP_SLOTS)
            .chain(REQUIRED_HOME_STARTUP_SLOTS)
            .map(|slot| (*slot).to_owned())
            .chain(["view:home".to_owned()])
            .collect::<Vec<_>>();

        assert!(startup_slots_ready(&slots));

        let mut diagnostic_slots = slots.clone();
        diagnostic_slots.push("health.hero.invisible".into());
        assert!(startup_slots_ready(&diagnostic_slots));

        for optional_slot in [
            "home.hero.foreground",
            "conversation.banner.foreground",
            "sidebar.header.background",
            "sidebar.header.decoration",
        ] {
            assert!(
                !REQUIRED_GLOBAL_STARTUP_SLOTS.contains(&optional_slot)
                    && !REQUIRED_COMPOSER_STARTUP_SLOTS.contains(&optional_slot)
                    && !REQUIRED_HOME_STARTUP_SLOTS.contains(&optional_slot)
                    && !REQUIRED_COMPACT_HOME_STARTUP_SLOTS.contains(&optional_slot)
                    && !REQUIRED_CONVERSATION_STARTUP_SLOTS.contains(&optional_slot),
                "{optional_slot} must stay optional"
            );
        }

        let mut missing_core = slots;
        missing_core.retain(|slot| slot != "composer.editor");
        assert!(!startup_slots_ready(&missing_core));
        assert_eq!(missing_startup_slots(&missing_core), ["composer.editor"]);

        let conversation_slots = REQUIRED_GLOBAL_STARTUP_SLOTS
            .iter()
            .chain(REQUIRED_COMPOSER_STARTUP_SLOTS)
            .chain(REQUIRED_CONVERSATION_STARTUP_SLOTS)
            .map(|slot| (*slot).to_owned())
            .chain(["view:conversation".to_owned()])
            .collect::<Vec<_>>();
        assert!(startup_slots_ready(&conversation_slots));

        let compact_home_slots = REQUIRED_GLOBAL_STARTUP_SLOTS
            .iter()
            .chain(REQUIRED_COMPOSER_STARTUP_SLOTS)
            .chain(REQUIRED_COMPACT_HOME_STARTUP_SLOTS)
            .map(|slot| (*slot).to_owned())
            .chain(["view:home-compact".to_owned()])
            .collect::<Vec<_>>();
        assert!(startup_slots_ready(&compact_home_slots));

        let missing_view = REQUIRED_GLOBAL_STARTUP_SLOTS
            .iter()
            .chain(REQUIRED_COMPOSER_STARTUP_SLOTS)
            .map(|slot| (*slot).to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            missing_startup_slots(&missing_view),
            [REQUIRED_STARTUP_VIEW]
        );
    }

    #[test]
    fn stopping_an_exited_codex_session_succeeds() {
        let runtime = runtime_with_exited_session();

        assert!(stop_theme_preview(&runtime).expect("stopping exited preview"));
        assert!(runtime.session.lock().expect("theme session").is_none());
    }

    #[test]
    fn stale_expiry_cannot_stop_a_newer_preview_session() {
        let runtime = runtime_with_exited_session();

        assert!(!stop_theme_preview_if_session(&runtime, 2).expect("ignore stale expiry"));
        assert!(runtime.session.lock().expect("theme session").is_some());
        assert!(stop_theme_preview_if_session(&runtime, 1).expect("stop matching preview"));
    }

    #[test]
    fn expired_development_preview_clears_its_session() {
        let runtime = runtime_with_exited_session();
        let mut session = runtime.session.lock().expect("theme session");
        let preview = session.as_mut().expect("preview session");
        preview.deadline = Some(Instant::now() - Duration::from_secs(1));
        preview.report.source = ThemePreviewSource::LocalDevelopment;
        drop(session);

        assert!(
            runtime
                .current_preview()
                .expect("expired preview")
                .is_none()
        );
        assert!(runtime.session.lock().expect("theme session").is_none());
    }

    fn dispatch_mouse_move<S>(socket: &mut tungstenite::WebSocket<S>, id: u64, x: f64, y: f64)
    where
        S: Read + Write,
    {
        socket
            .send(Message::Text(
                json!({
                    "id": id,
                    "method": "Input.dispatchMouseEvent",
                    "params": {
                        "type": "mouseMoved",
                        "x": x,
                        "y": y,
                        "button": "none",
                        "buttons": 0,
                        "pointerType": "mouse"
                    }
                })
                .to_string()
                .into(),
            ))
            .expect("dispatch mouse move");

        loop {
            let message = socket.read().expect("read mouse move response");
            if !message.is_text() {
                continue;
            }
            let response: Value =
                serde_json::from_str(message.to_text().expect("mouse move response text"))
                    .expect("mouse move response JSON");
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            assert!(response.get("error").is_none(), "{response}");
            break;
        }
    }

    fn dispatch_mouse_click<S>(socket: &mut tungstenite::WebSocket<S>, id: u64, x: f64, y: f64)
    where
        S: Read + Write,
    {
        for (offset, event_type) in ["mousePressed", "mouseReleased"].into_iter().enumerate() {
            let command_id = id + offset as u64;
            socket
                .send(Message::Text(
                    json!({
                        "id": command_id,
                        "method": "Input.dispatchMouseEvent",
                        "params": {
                            "type": event_type,
                            "x": x,
                            "y": y,
                            "button": "left",
                            "buttons": if event_type == "mousePressed" { 1 } else { 0 },
                            "clickCount": 1,
                            "pointerType": "mouse"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .expect("dispatch mouse click");
            loop {
                let message = socket.read().expect("read mouse click response");
                if !message.is_text() {
                    continue;
                }
                let response: Value =
                    serde_json::from_str(message.to_text().expect("mouse click response text"))
                        .expect("mouse click response JSON");
                if response.get("id").and_then(Value::as_u64) != Some(command_id) {
                    continue;
                }
                assert!(response.get("error").is_none(), "{response}");
                break;
            }
        }
    }

    fn dispatch_escape<S>(socket: &mut tungstenite::WebSocket<S>, id: u64)
    where
        S: Read + Write,
    {
        for (offset, event_type) in ["keyDown", "keyUp"].into_iter().enumerate() {
            let command_id = id + offset as u64;
            socket
                .send(Message::Text(
                    json!({
                        "id": command_id,
                        "method": "Input.dispatchKeyEvent",
                        "params": {
                            "type": event_type,
                            "key": "Escape",
                            "code": "Escape",
                            "windowsVirtualKeyCode": 27,
                            "nativeVirtualKeyCode": 53
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .expect("dispatch escape key");
            loop {
                let message = socket.read().expect("read escape response");
                if !message.is_text() {
                    continue;
                }
                let response: Value =
                    serde_json::from_str(message.to_text().expect("escape response text"))
                        .expect("escape response JSON");
                if response.get("id").and_then(Value::as_u64) != Some(command_id) {
                    continue;
                }
                assert!(response.get("error").is_none(), "{response}");
                break;
            }
        }
    }

    fn set_device_metrics<S>(
        socket: &mut tungstenite::WebSocket<S>,
        id: u64,
        width: Option<u64>,
        height: u64,
    ) where
        S: Read + Write,
    {
        let (method, params) = if let Some(width) = width {
            (
                "Emulation.setDeviceMetricsOverride",
                json!({
                    "width": width,
                    "height": height,
                    "deviceScaleFactor": 1,
                    "mobile": false
                }),
            )
        } else {
            ("Emulation.clearDeviceMetricsOverride", json!({}))
        };
        socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": params })
                    .to_string()
                    .into(),
            ))
            .expect("set device metrics");
        loop {
            let message = socket.read().expect("read device metrics response");
            if !message.is_text() {
                continue;
            }
            let response: Value =
                serde_json::from_str(message.to_text().expect("device metrics response text"))
                    .expect("device metrics response JSON");
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            assert!(response.get("error").is_none(), "{response}");
            break;
        }
    }

    #[test]
    fn settings_adapter_uses_language_independent_selector() {
        let adapter = compatibility::builtin_adapter("26.715.21316");
        assert_eq!(
            adapter.selectors.settings_item,
            "[data-settings-panel-slug]"
        );
        assert!(!adapter.selectors.settings_item.contains("aria-label"));
    }

    #[test]
    fn settings_slots_cover_cards_rows_and_switch_parts_without_localized_copy() {
        let source = include_str!("codex.rs");

        for slot in [
            "settings.surface",
            "settings.frame",
            "settings.canvas",
            "settings.toolbar",
            "settings.body",
            "settings.section",
            "settings.section.title",
            "settings.card",
            "settings.row",
            "settings.row.title",
            "settings.row.description",
            "settings.row.separator",
            "settings.switch",
            "settings.switch.checked",
            "settings.switch.track",
            "settings.switch.track.checked",
            "settings.switch.thumb",
        ] {
            assert!(source.contains(&format!("'{slot}'")), "missing {slot}");
        }
        assert!(source.contains("control.firstElementChild"));
        assert!(source.contains("control.firstElementChild?.firstElementChild"));
        assert!(source.contains("item.getAttribute('aria-current') === 'page'"));
        assert!(source.contains("markSettings(settingsItems, appMain, slots)"));
        assert!(source.contains("const settingsContent = legacyContent ?? settingsMain"));
        assert!(
            source.contains("settingsContent.querySelector(adapter.selectors.mainContentFrame)")
        );
        assert!(source.contains("const toolbar = [...canvas.children].find"));
        assert!(source.contains("const settingsOpen = Boolean"));
        assert!(source.contains("settingsOpen && target?.closest(adapter.selectors.main)"));
        assert!(source.contains("parseFloat(style.borderTopLeftRadius) >= 8"));
        assert!(source.contains("isCheckedControl(control)"));
        for localized_copy in ["设置", "Settings"] {
            assert!(!source.contains(&format!("textContent === '{localized_copy}'")));
        }
    }

    #[test]
    fn menu_slots_use_aria_roles_and_refresh_dynamic_state() {
        let source = include_str!("codex.rs");

        assert!(source.contains("document.querySelectorAll('[role=\"menu\"]')"));
        assert!(source.contains(
            "'[role=\"menuitem\"], [role=\"menuitemcheckbox\"], [role=\"menuitemradio\"]'"
        ));
        assert!(source.contains("item.hasAttribute('data-highlighted')"));
        assert!(source.contains("menu.querySelectorAll('[role=\"separator\"], hr')"));
        assert!(source.contains("'data-highlighted'"));
        assert!(source.contains("'data-state'"));
    }

    #[test]
    fn main_fade_uses_versioned_adapter_selector() {
        let adapter = compatibility::builtin_adapter("26.715.21316");
        assert_eq!(
            adapter.selectors.main_top_fade.as_deref(),
            Some(".app-shell-main-content-top-fade")
        );
    }

    #[test]
    #[ignore = "launches an isolated themed ChatGPT App window"]
    #[cfg(target_os = "macos")]
    fn external_theme_hides_native_main_fade() {
        let installation = detect().expect("ChatGPT App installation");
        let mut instance = start_isolated(&installation).expect("isolated ChatGPT App");
        let (port, _) = wait_for_devtools(instance._profile.path(), &mut instance.process)
            .expect("local theme channel");
        let target = wait_for_codex_target(port, &mut instance.process).expect("ChatGPT App page");
        let adapter = compatibility::builtin_adapter(&installation.version);
        let fade_selector = adapter
            .selectors
            .main_top_fade
            .as_deref()
            .expect("main fade selector");
        let themes = theme::ThemeRepository::new(
            tempfile::tempdir()
                .expect("temporary directory")
                .path()
                .to_path_buf(),
        )
        .expect("theme repository");
        let package = themes
            .load_development(&external_test_theme())
            .expect("external theme");
        let expression = format!(
            r#"(async () => {{
              const editor = document.querySelector('[data-codex-composer], textarea, [contenteditable="true"]');
              if (editor instanceof HTMLTextAreaElement) {{
                const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')?.set;
                setter?.call(editor, 'retheme fade probe');
                editor.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: 'retheme fade probe' }}));
              }} else if (editor instanceof HTMLElement) {{
                editor.focus();
                document.execCommand('insertText', false, 'retheme fade probe');
                editor.dispatchEvent(new InputEvent('input', {{ bubbles: true, inputType: 'insertText', data: 'retheme fade probe' }}));
              }}
              await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
              const fade = document.querySelector({fade_selector});
              if (!fade) return {{
                exists: false,
                display: null,
                candidates: [...document.querySelectorAll('[class]')]
                  .filter(node => /fade|gradient|mask|scroll/i.test(String(node.className)))
                  .map(node => ({{
                    tag: node.tagName,
                    className: String(node.className),
                    rect: node.getBoundingClientRect().toJSON(),
                    background: getComputedStyle(node).background,
                    boxShadow: getComputedStyle(node).boxShadow
                  }}))
              }};
              document.documentElement.setAttribute('data-ct-theme', {theme_id});
              fade.setAttribute('data-ct-slot', 'main.fade');
              const style = document.createElement('style');
              style.textContent = {css};
              document.head.append(style);
              return {{ exists: true, display: getComputedStyle(fade).display }};
            }})()"#,
            fade_selector = serde_json::to_string(fade_selector).expect("fade selector JSON"),
            theme_id = serde_json::to_string(package.id()).expect("theme id JSON"),
            css = serde_json::to_string(package.css()).expect("theme CSS JSON"),
        );
        let (mut socket, _) = connect(&target.web_socket_debugger_url).expect("local theme socket");
        let state = evaluate_value(&mut socket, 1, &expression).expect("main fade state");
        assert!(
            state["exists"] == false || state["display"] == "none",
            "the native fade may be absent, but must be hidden when present: {state}"
        );
        let _ = socket.close(None);
    }

    #[test]
    #[ignore = "requires ChatGPT to be installed on the test Mac"]
    #[cfg(target_os = "macos")]
    fn detects_installed_codex() {
        let installation = detect().expect("Codex should be installed on the test Mac");
        assert_eq!(installation.bundle_id, CODEX_BUNDLE_ID);
        assert!(installation.executable.is_file());
    }

    #[test]
    #[ignore = "requires ChatGPT to be installed on the test PC"]
    #[cfg(target_os = "windows")]
    fn detects_installed_chatgpt_package() {
        let installation = detect().expect("ChatGPT should be installed on the test PC");
        assert_eq!(installation.app_name, "ChatGPT");
        assert!(installation.bundle_id.starts_with("OpenAI.Codex_"));
        assert!(
            installation
                .app_user_model_id
                .starts_with(&format!("{}!", installation.bundle_id))
        );
        assert!(installation.executable.ends_with("app\\ChatGPT.exe"));
        assert!(installation.executable.is_file());
    }

    #[test]
    #[ignore = "launches an isolated Microsoft Store ChatGPT window"]
    #[cfg(target_os = "windows")]
    fn completes_isolated_cdp_smoke_test_on_windows() {
        let report = run_smoke_test().expect("isolated smoke test should pass");
        assert!(report.loopback_only);
        assert!(report.probe_applied);
        assert!(report.probe_removed);
    }

    #[test]
    #[ignore = "launches an isolated Codex window"]
    #[cfg(target_os = "macos")]
    fn completes_isolated_cdp_smoke_test() {
        let report = run_smoke_test().expect("isolated smoke test should pass");
        assert!(report.loopback_only);
        assert!(report.probe_applied);
        assert!(report.probe_removed);
    }

    #[test]
    #[ignore = "launches two isolated themed Codex windows"]
    #[cfg(target_os = "macos")]
    fn starts_another_theme_after_codex_window_closes() {
        let runtime = ThemeRuntime::default();
        let themes = theme::ThemeRepository::new(
            tempfile::tempdir()
                .expect("temporary directory")
                .path()
                .to_path_buf(),
        )
        .expect("theme repository");
        let compatibility_directory = tempfile::tempdir().expect("compatibility directory");
        let compatibility = compatibility::CompatibilityRepository::new(
            compatibility_directory.path().to_path_buf(),
        )
        .expect("compatibility repository");

        let theme_path = external_test_theme();
        start_development_theme_preview(
            &runtime,
            &themes,
            &compatibility,
            &theme_path,
            None,
            false,
            "zh-CN",
        )
        .expect("first theme preview should start");
        let websocket_url = {
            let mut session = runtime.session.lock().expect("theme session");
            let session = session.as_mut().expect("active theme session");
            wait_for_codex_target(session.port, &mut session.instance.process)
                .expect("Codex page")
                .web_socket_debugger_url
        };
        let (mut socket, _) = connect(&websocket_url).expect("CDP socket");
        socket
            .send(Message::Text(
                json!({ "id": 1, "method": "Page.close" })
                    .to_string()
                    .into(),
            ))
            .expect("close Codex window");
        drop(socket);

        let started = Instant::now();
        while runtime.current_preview().expect("preview status").is_some() {
            assert!(
                started.elapsed() < STATUS_PROBE_GRACE_PERIOD + Duration::from_secs(5),
                "closed Codex window should clear its theme session"
            );
            thread::sleep(Duration::from_millis(100));
        }

        let report = start_development_theme_preview(
            &runtime,
            &themes,
            &compatibility,
            &theme_path,
            None,
            false,
            "zh-CN",
        )
        .expect("second theme preview should start");
        assert!(matches!(
            report.source,
            ThemePreviewSource::LocalDevelopment
        ));
        assert!(stop_theme_preview(&runtime).expect("second theme preview should stop"));
    }

    #[test]
    #[ignore = "launches an isolated themed Codex window"]
    #[cfg(target_os = "macos")]
    fn applies_and_restores_external_theme() {
        let runtime = ThemeRuntime::default();
        let themes = theme::ThemeRepository::new(
            tempfile::tempdir()
                .expect("temporary directory")
                .path()
                .to_path_buf(),
        )
        .expect("theme repository");
        let compatibility_directory = tempfile::tempdir().expect("compatibility directory");
        let compatibility = compatibility::CompatibilityRepository::new(
            compatibility_directory.path().to_path_buf(),
        )
        .expect("compatibility repository");
        let theme_path = external_test_theme();
        let report = start_development_theme_preview(
            &runtime,
            &themes,
            &compatibility,
            &theme_path,
            None,
            false,
            "zh-CN",
        )
        .expect("theme preview should start");
        assert!(report.applied_slots.iter().any(|slot| slot == "titlebar"));
        assert!(report.applied_slots.iter().any(|slot| slot == "sidebar"));
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "sidebar.item")
        );
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "sidebar.item.icon")
        );
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "sidebar.item.label")
        );
        assert!(report.applied_slots.iter().any(|slot| slot == "composer"));
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "composer.editor")
        );
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "composer.permission")
        );
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "composer.submit")
        );
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "sidebar.section.projects")
        );
        assert!(report.applied_slots.iter().any(|slot| slot == "home.hero"));
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "home.hero.copy")
        );
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "home.hero.divider")
        );
        assert!(report.applied_slots.iter().any(|slot| slot == "home.cards"));
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "home.card.background")
        );
        let websocket_url = {
            let mut session = runtime.session.lock().expect("theme session");
            let session = session.as_mut().expect("active theme session");
            wait_for_codex_target(session.port, &mut session.instance.process)
                .expect("Codex page")
                .web_socket_debugger_url
        };
        let (mut socket, _) = connect(&websocket_url).expect("CDP socket");
        let context_colors = evaluate_value(
                &mut socket,
                1,
                r#"(() => {
                  const context = document.querySelector('[data-ct-slot="composer.context"]');
                  const outer = context?.parentElement;
                  const contextRect = context?.getBoundingClientRect();
                  const outerRect = outer?.getBoundingClientRect();
                  const contextStyle = context ? getComputedStyle(context) : null;
                  return {
                    exists: Boolean(context),
                    contextBackground: contextStyle?.backgroundColor,
                    outerBackground: outer ? getComputedStyle(outer).backgroundColor : null,
                    radius: contextStyle ? parseFloat(contextStyle.borderTopLeftRadius) : 0,
                    insetLeft: contextRect && outerRect ? contextRect.left > outerRect.left : false,
                    insetRight: contextRect && outerRect ? contextRect.right < outerRect.right : false
                  };
                })()"#,
            )
            .expect("composer context geometry");
        if context_colors["exists"] == true {
            assert_ne!(context_colors["contextBackground"], "rgba(0, 0, 0, 0)");
            assert_eq!(context_colors["outerBackground"], "rgba(0, 0, 0, 0)");
            assert!(context_colors["radius"].as_f64().expect("context radius") > 0.0);
            assert_eq!(context_colors["insetLeft"], true);
            assert_eq!(context_colors["insetRight"], true);
        }
        let hero_layout = evaluate_value(
            &mut socket,
            9,
            r#"(() => {
                  const header = document.querySelector('[data-ct-slot="titlebar"]');
                  const hero = document.querySelector('[data-ct-mount="home.hero"]');
                  return {
                    headerBottom: header.getBoundingClientRect().bottom,
                    heroTop: hero.getBoundingClientRect().top
                  };
                })()"#,
        )
        .expect("hero header geometry");
        assert!(
            hero_layout["heroTop"].as_f64().expect("hero top")
                >= hero_layout["headerBottom"].as_f64().expect("header bottom"),
            "banner must start below the fixed header"
        );
        let before = evaluate_value(
                &mut socket,
                10,
                r#"new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => {
                  const card = document.querySelector('[data-ct-slot="home.card"]');
                  const rect = card.getBoundingClientRect();
                  window.__codexThemeTestStyle = document.getElementById('codex-theme-runtime-style');
                  resolve({
                    x: rect.left + rect.width / 2,
                    y: rect.top + rect.height / 2
                  });
                })))"#,
            )
            .expect("card geometry before hover");
        dispatch_mouse_move(
            &mut socket,
            11,
            before["x"].as_f64().expect("card center x"),
            before["y"].as_f64().expect("card center y"),
        );
        let after = evaluate_value(
                &mut socket,
                12,
                r#"new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => {
                  const card = document.querySelector('[data-ct-slot="home.card"]');
                  const rect = card.getBoundingClientRect();
                  resolve({
                    hovered: card.matches(':hover'),
                    sameStyle: document.getElementById('codex-theme-runtime-style') === window.__codexThemeTestStyle
                  });
                })))"#,
            )
            .expect("card geometry after hover");
        assert_eq!(after["hovered"], true);
        assert_eq!(after["sameStyle"], true);
        let _ = socket.close(None);

        assert!(stop_theme_preview(&runtime).expect("theme preview should stop"));
    }

    #[test]
    #[ignore = "launches an isolated themed ChatGPT App window and drafts on home"]
    #[cfg(target_os = "macos")]
    fn switches_home_to_compact_banner_when_drafting() {
        let runtime = ThemeRuntime::default();
        let themes = theme::ThemeRepository::new(
            tempfile::tempdir()
                .expect("temporary directory")
                .path()
                .to_path_buf(),
        )
        .expect("theme repository");
        let compatibility_directory = tempfile::tempdir().expect("compatibility directory");
        let compatibility = compatibility::CompatibilityRepository::new(
            compatibility_directory.path().to_path_buf(),
        )
        .expect("compatibility repository");
        let theme_path = external_test_theme();
        let report = start_development_theme_preview(
            &runtime,
            &themes,
            &compatibility,
            &theme_path,
            None,
            false,
            "zh-CN",
        )
        .expect("theme preview should start");
        assert!(
            report
                .applied_slots
                .iter()
                .any(|slot| slot == "home.prompt.title")
        );

        let websocket_url = {
            let mut session = runtime.session.lock().expect("theme session");
            let session = session.as_mut().expect("active theme session");
            wait_for_codex_target(session.port, &mut session.instance.process)
                .expect("ChatGPT App page")
                .web_socket_debugger_url
        };
        let (mut socket, _) = connect(&websocket_url).expect("local theme socket");
        let prompt = evaluate_value(
            &mut socket,
            1,
            r#"(() => {
              const title = document.querySelector('[data-ct-mount="home.prompt.title"]');
              const native = [...document.querySelectorAll('[data-ct-home-prompt-native]')];
              const hero = document.querySelector('[data-ct-mount="home.hero"]');
              const heroViewport = hero?.querySelector(
                ':scope > [data-ct-slot="home.hero.viewport"]'
              );
              const stage = document.querySelector('[data-ct-slot="home.stage"]');
              const prompt = document.querySelector('[data-ct-slot="home.prompt"]');
              const cards = document.querySelector('[data-ct-slot="home.cards"]');
              const cardItems = [...document.querySelectorAll('[data-ct-slot="home.card"]')]
                .filter(card => getComputedStyle(card).display !== 'none');
              const heroRect = hero?.getBoundingClientRect();
              const heroViewportRect = heroViewport?.getBoundingClientRect();
              const stageRect = stage?.getBoundingClientRect();
              const promptRect = prompt?.getBoundingClientRect();
              const cardsRect = cards?.getBoundingClientRect();
              const cardRects = cardItems.map(card => card.getBoundingClientRect());
              const layout = document.querySelector('[data-ct-slot="home.layout"]');
              const contentRegion = document.querySelector(
                '[data-ct-slot="home.content.region"]'
              );
              const composerRegion = document.querySelector('[data-ct-slot="composer.region"]');
              const contentRegionRect = contentRegion?.getBoundingClientRect();
              return {
                title: title?.textContent,
                nativeCount: native.length,
                nativeHidden: native.every(node => getComputedStyle(node).display === 'none'),
                heroFirstInStage: hero?.parentElement === stage
                  && hero === stage?.firstElementChild,
                heroPosition: hero ? getComputedStyle(hero).position : null,
                heroFitsStage: Boolean(heroRect && stageRect
                  && heroRect.width <= stageRect.width + 1
                  && heroRect.left >= stageRect.left - 1
                  && heroRect.right <= stageRect.right + 1),
                heroMaxWidth: hero ? getComputedStyle(hero).maxWidth : null,
                heroViewportBelongsToHero: heroViewport?.parentElement === hero,
                heroViewportInFlow: Boolean(heroViewport
                  && !['absolute', 'fixed'].includes(getComputedStyle(heroViewport).position)),
                heroViewportFillsHero: Boolean(heroRect && heroViewportRect
                  && Math.abs(heroViewportRect.width - heroRect.width) <= 1
                  && Math.abs(heroViewportRect.height - heroRect.height) <= 1),
                heroPrecedesPrompt: Boolean(heroRect && promptRect
                  && heroRect.bottom <= promptRect.top + 1),
                cardsFollowHero: Boolean(heroRect && cardsRect
                  && cardsRect.top >= heroRect.bottom),
                cardItemsFollowHero: Boolean(heroRect && cardRects.length
                  && cardRects.every(rect => rect.top >= heroRect.bottom)),
                promptCentered: Boolean(promptRect && stageRect
                  && Math.abs(
                    promptRect.left + promptRect.width / 2
                      - (stageRect.left + stageRect.width / 2)
                  ) <= 1),
                contentRegionContainsStage: stage?.parentElement === contentRegion,
                contentRegionJustify: contentRegion
                  ? getComputedStyle(contentRegion).justifyContent
                  : null,
                contentRegionDirection: contentRegion
                  ? getComputedStyle(contentRegion).flexDirection
                  : null,
                contentRegionAlignItems: contentRegion
                  ? getComputedStyle(contentRegion).alignItems
                  : null,
                stageDirection: stage ? getComputedStyle(stage).flexDirection : null,
                stageFitsRegion: Boolean(stageRect && contentRegionRect
                  && stageRect.left >= contentRegionRect.left - 1
                  && stageRect.right <= contentRegionRect.right + 1),
                composerIsSeparateRegion: Boolean(composerRegion
                  && !contentRegion?.contains(composerRegion))
              };
            })()"#,
        )
        .expect("custom home prompt");
        assert!(
            prompt["title"]
                .as_str()
                .is_some_and(|title| !title.trim().is_empty()),
            "custom home prompt is missing: {prompt}"
        );
        assert!(prompt["nativeCount"].as_u64().unwrap_or_default() > 0);
        assert_eq!(prompt["nativeHidden"], true);
        assert_eq!(prompt["heroFirstInStage"], true, "{prompt}");
        assert_eq!(prompt["heroPosition"], "relative", "{prompt}");
        assert_eq!(prompt["heroFitsStage"], true, "{prompt}");
        assert_eq!(prompt["heroMaxWidth"], "1080px", "{prompt}");
        assert_eq!(prompt["heroViewportBelongsToHero"], true, "{prompt}");
        assert_eq!(prompt["heroViewportInFlow"], true, "{prompt}");
        assert_eq!(prompt["heroViewportFillsHero"], true, "{prompt}");
        assert_eq!(prompt["heroPrecedesPrompt"], true, "{prompt}");
        assert_eq!(prompt["cardsFollowHero"], true, "{prompt}");
        assert_eq!(prompt["cardItemsFollowHero"], true, "{prompt}");
        assert_eq!(prompt["promptCentered"], true, "{prompt}");
        assert_eq!(prompt["contentRegionContainsStage"], true, "{prompt}");
        assert_eq!(prompt["contentRegionJustify"], "center", "{prompt}");
        assert_eq!(prompt["contentRegionDirection"], "row", "{prompt}");
        assert_eq!(prompt["contentRegionAlignItems"], "flex-end", "{prompt}");
        assert_eq!(prompt["stageDirection"], "column", "{prompt}");
        assert_eq!(prompt["stageFitsRegion"], true, "{prompt}");
        assert_eq!(prompt["composerIsSeparateRegion"], true, "{prompt}");
        let drafted = evaluate(
            &mut socket,
            2,
            r#"(() => {
              const surface = document.querySelector('[data-ct-slot="composer.editor"]');
              const editor = surface?.matches('textarea, input, [contenteditable="true"]')
                ? surface
                : surface?.querySelector('textarea, input, [contenteditable="true"]');
              if (editor instanceof HTMLTextAreaElement || editor instanceof HTMLInputElement) {
                const prototype = editor instanceof HTMLTextAreaElement
                  ? HTMLTextAreaElement.prototype
                  : HTMLInputElement.prototype;
                const setter = Object.getOwnPropertyDescriptor(prototype, 'value')?.set;
                setter?.call(editor, 'retheme compact layout probe');
                editor.dispatchEvent(new InputEvent('input', {
                  bubbles: true,
                  inputType: 'insertText',
                  data: 'retheme compact layout probe'
                }));
                return true;
              }
              if (editor instanceof HTMLElement) {
                editor.focus();
                document.execCommand('insertText', false, 'retheme compact layout probe');
                editor.dispatchEvent(new InputEvent('input', {
                  bubbles: true,
                  inputType: 'insertText',
                  data: 'retheme compact layout probe'
                }));
                return true;
              }
              return false;
            })()"#,
        )
        .expect("draft home composer");
        assert!(drafted, "home composer must accept draft text");
        let compact_started = Instant::now();
        let mut compact_command_id = 3;
        let compact_home_layout = loop {
            let state = evaluate_value(
                &mut socket,
                compact_command_id,
                r#"new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => {
              const main = document.querySelector('[data-ct-slot="main"]');
              const layout = document.querySelector('[data-ct-slot="home.layout"]');
              const stage = document.querySelector('[data-ct-slot="home.stage"]');
              const composerRegion = document.querySelector('[data-ct-slot="composer.region"]');
              const composerRoot = document.querySelector('[data-codex-composer-root]');
              const composer = document.querySelector('[data-ct-slot="composer"]');
              const composerDecoration = document.querySelector(
                '[data-ct-mount="composer.decoration"]'
              );
              const banner = document.querySelector('[data-ct-mount="conversation.banner"]');
              const mainRect = main?.getBoundingClientRect();
              const layoutRect = layout?.getBoundingClientRect();
              const stageRect = stage?.getBoundingClientRect();
              const composerRegionRect = composerRegion?.getBoundingClientRect();
              const composerRect = composer?.getBoundingClientRect();
              const composerDecorationRect = composerDecoration?.getBoundingClientRect();
              const cardsContainer = document.querySelector('[data-ct-slot="home.cards"]');
              const prompt = document.querySelector('[data-ct-slot="home.prompt"]');
              const bannerRect = banner?.getBoundingClientRect();
              const foreground = banner?.querySelector(
                '[data-ct-slot="conversation.banner.foreground"]'
              );
              const foregroundRect = foreground?.getBoundingClientRect();
              let foregroundClipTop = window.visualViewport?.offsetTop ?? 0;
              for (
                let ancestor = banner?.parentElement;
                ancestor;
                ancestor = ancestor.parentElement
              ) {
                if (!['auto', 'scroll', 'hidden', 'clip'].includes(
                  getComputedStyle(ancestor).overflowY
                )) continue;
                foregroundClipTop = Math.max(
                  foregroundClipTop,
                  ancestor.getBoundingClientRect().top + ancestor.clientTop
                );
              }
              resolve({
                view: document.documentElement.getAttribute('data-ct-view'),
                layoutCount: document.querySelectorAll('[data-ct-slot="home.layout"]').length,
                stageCount: document.querySelectorAll('[data-ct-slot="home.stage"]').length,
                composerRegionCount: document.querySelectorAll(
                  '[data-ct-slot="composer.region"]'
                ).length,
                bannerInLayout: banner?.parentElement === layout,
                bannerInStage: Boolean(stage?.contains(banner)),
                stageContainsComposer: Boolean(stage?.contains(composerRoot)),
                layoutContainsStage: Boolean(layout?.contains(stage)),
                layoutContainsComposer: Boolean(layout?.contains(composerRoot)),
                regionContainsComposer: Boolean(composerRegion?.contains(composerRoot)),
                heroExists: Boolean(document.querySelector('[data-ct-mount="home.hero"]')),
                bannerHeight: bannerRect?.height ?? 0,
                bannerBottom: bannerRect?.bottom ?? null,
                bannerCenter: bannerRect ? bannerRect.left + bannerRect.width / 2 : 0,
                foregroundTop: foregroundRect?.top ?? null,
                foregroundBottom: foregroundRect?.bottom ?? null,
                foregroundClipTop,
                layoutCenter: layoutRect ? layoutRect.left + layoutRect.width / 2 : 0,
                mainHeight: mainRect?.height ?? 0,
                layoutHeight: layoutRect?.height ?? 0,
                layoutBottom: layoutRect?.bottom ?? 0,
                stageHeight: stageRect?.height ?? 0,
                composerRegionBottom: composerRegionRect?.bottom ?? 0,
                composerDecorationLocalUrl: composerDecoration?.querySelector('img')?.src
                  .startsWith('http://127.0.0.1:') ?? false,
                composerDecorationCentered: Boolean(composerRect && composerDecorationRect
                  && Math.abs(
                    composerRect.left + composerRect.width / 2
                      - composerDecorationRect.left - composerDecorationRect.width / 2
                  ) <= 2),
                composerDecorationNonInteractive: composerDecoration
                  ? getComputedStyle(composerDecoration).pointerEvents === 'none'
                  : false,
                cardsHidden: !cardsContainer || getComputedStyle(cardsContainer).display === 'none',
                promptHidden: !prompt || getComputedStyle(prompt).display === 'none'
              });
            })))"#,
            )
            .expect("compact home layout");
            compact_command_id += 1;
            if state["view"] == "home-compact" {
                break state;
            }
            assert!(
                compact_started.elapsed() < Duration::from_secs(5),
                "drafted home state did not stabilize: {state}"
            );
            thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(
            compact_home_layout["view"], "home-compact",
            "compact home state: {compact_home_layout}"
        );
        assert_eq!(compact_home_layout["layoutCount"], 1);
        assert_eq!(compact_home_layout["stageCount"], 1);
        assert_eq!(compact_home_layout["composerRegionCount"], 1);
        assert_eq!(
            compact_home_layout["bannerInLayout"], true,
            "compact banner must mount in the shared home layout: {compact_home_layout}"
        );
        assert_eq!(compact_home_layout["bannerInStage"], false);
        assert_eq!(compact_home_layout["stageContainsComposer"], false);
        assert_eq!(compact_home_layout["layoutContainsStage"], true);
        assert_eq!(compact_home_layout["layoutContainsComposer"], true);
        assert_eq!(compact_home_layout["regionContainsComposer"], true);
        assert_eq!(compact_home_layout["heroExists"], false);
        assert!(
            compact_home_layout["bannerHeight"]
                .as_f64()
                .is_some_and(|height| height > 0.0 && height <= 220.0)
        );
        assert!(
            compact_home_layout["bannerCenter"]
                .as_f64()
                .zip(compact_home_layout["layoutCenter"].as_f64())
                .is_some_and(|(banner, layout)| (banner - layout).abs() <= 1.0),
            "compact banner must be horizontally centered: {compact_home_layout}"
        );
        assert!(
            compact_home_layout["foregroundTop"]
                .as_f64()
                .zip(compact_home_layout["foregroundClipTop"].as_f64())
                .is_some_and(|(foreground, clip)| foreground >= clip - 1.0),
            "compact banner foreground must stay below its clipping edge: {compact_home_layout}"
        );
        assert!(
            compact_home_layout["foregroundBottom"]
                .as_f64()
                .zip(compact_home_layout["bannerBottom"].as_f64())
                .is_some_and(|(foreground, banner)| (foreground - banner).abs() <= 2.0),
            "compact banner foreground must remain visible and bottom-aligned: {compact_home_layout}"
        );
        assert_eq!(compact_home_layout["cardsHidden"], true);
        assert_eq!(compact_home_layout["promptHidden"], true);
        assert_eq!(compact_home_layout["composerDecorationLocalUrl"], true);
        assert_eq!(compact_home_layout["composerDecorationCentered"], true);
        assert_eq!(
            compact_home_layout["composerDecorationNonInteractive"],
            true
        );
        assert!(
            compact_home_layout["layoutHeight"]
                .as_f64()
                .zip(compact_home_layout["mainHeight"].as_f64())
                .is_some_and(|(layout, main)| layout >= main - 64.0),
            "compact home layout must keep the viewport height: {compact_home_layout}"
        );
        assert!(
            compact_home_layout["composerRegionBottom"]
                .as_f64()
                .zip(compact_home_layout["layoutBottom"].as_f64())
                .is_some_and(|(composer, layout)| composer >= layout - 20.0),
            "compact composer region must remain at the bottom: {compact_home_layout}"
        );

        let _ = socket.close(None);

        assert!(stop_theme_preview(&runtime).expect("theme preview should stop"));
    }

    #[test]
    #[ignore = "launches an isolated themed ChatGPT App window and expands a home card"]
    #[cfg(target_os = "macos")]
    fn keeps_home_composer_at_bottom_after_expanding_card() {
        let runtime = ThemeRuntime::default();
        let themes = theme::ThemeRepository::new(
            tempfile::tempdir()
                .expect("temporary directory")
                .path()
                .to_path_buf(),
        )
        .expect("theme repository");
        let compatibility_directory = tempfile::tempdir().expect("compatibility directory");
        let compatibility = compatibility::CompatibilityRepository::new(
            compatibility_directory.path().to_path_buf(),
        )
        .expect("compatibility repository");
        let theme_path = external_test_theme();
        start_development_theme_preview(
            &runtime,
            &themes,
            &compatibility,
            &theme_path,
            None,
            false,
            "zh-CN",
        )
        .expect("theme preview should start");
        let websocket_url = {
            let mut session = runtime.session.lock().expect("theme session");
            let session = session.as_mut().expect("active theme session");
            wait_for_codex_target(session.port, &mut session.instance.process)
                .expect("ChatGPT App page")
                .web_socket_debugger_url
        };
        let (mut socket, _) = connect(&websocket_url).expect("local theme socket");
        let clicked = evaluate(
            &mut socket,
            1,
            r#"(() => {
              const card = [...document.querySelectorAll('[data-ct-slot="home.card"]')]
                .find(node => node.getBoundingClientRect().height > 0);
              card?.click();
              return Boolean(card);
            })()"#,
        )
        .expect("expand home card");
        assert!(clicked, "a home suggestion card should exist");
        let started = Instant::now();
        let mut command_id = 2;
        let state = loop {
            let state = evaluate_value(
                &mut socket,
                command_id,
                r#"new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(() => {
                  const layout = document.querySelector('[data-ct-slot="home.layout"]');
                  const stage = document.querySelector('[data-ct-slot="home.stage"]');
                  const region = document.querySelector('[data-ct-slot="composer.region"]');
                  const composerRoot = document.querySelector('[data-codex-composer-root]');
                  const layoutRect = layout?.getBoundingClientRect();
                  const regionRect = region?.getBoundingClientRect();
                  const expandedSectionCount = document.querySelectorAll('section').length;
                  resolve({
                    ready: ['home', 'home-compact'].includes(
                      document.documentElement.getAttribute('data-ct-view')
                    ) && Boolean(layout && stage && region && composerRoot)
                      && expandedSectionCount > 1,
                    view: document.documentElement.getAttribute('data-ct-view'),
                    stageContainsComposer: Boolean(stage?.contains(composerRoot)),
                    layoutHeight: layoutRect?.height ?? 0,
                    layoutBottom: layoutRect?.bottom ?? 0,
                    regionBottom: regionRect?.bottom ?? 0,
                    expandedSectionCount
                  });
                })))"#,
            )
            .expect("expanded home layout");
            command_id += 1;
            if state["ready"] == true {
                break state;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "expanded home state did not stabilize: {state}"
            );
            thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(state["stageContainsComposer"], false, "{state}");
        assert!(
            state["layoutHeight"]
                .as_f64()
                .is_some_and(|height| height > 700.0),
            "expanded home layout collapsed: {state}"
        );
        assert!(
            state["regionBottom"]
                .as_f64()
                .zip(state["layoutBottom"].as_f64())
                .is_some_and(|(region, layout)| region >= layout - 20.0),
            "expanded home composer moved upward: {state}"
        );
        assert!(state["expandedSectionCount"].as_u64().unwrap_or_default() > 0);
        let _ = socket.close(None);
        assert!(stop_theme_preview(&runtime).expect("theme preview should stop"));
    }

    #[test]
    #[ignore = "launches an isolated themed ChatGPT App window and inspects a history thread"]
    #[cfg(target_os = "macos")]
    fn opens_history_thread_with_conversation_layout() {
        let runtime = ThemeRuntime::default();
        let themes = theme::ThemeRepository::new(
            tempfile::tempdir()
                .expect("temporary directory")
                .path()
                .to_path_buf(),
        )
        .expect("theme repository");
        let compatibility_directory = tempfile::tempdir().expect("compatibility directory");
        let compatibility = compatibility::CompatibilityRepository::new(
            compatibility_directory.path().to_path_buf(),
        )
        .expect("compatibility repository");
        let theme_path = external_test_theme();
        start_development_theme_preview(
            &runtime,
            &themes,
            &compatibility,
            &theme_path,
            None,
            false,
            "zh-CN",
        )
        .expect("theme preview should start");
        let websocket_url = {
            let mut session = runtime.session.lock().expect("theme session");
            let session = session.as_mut().expect("active theme session");
            wait_for_codex_target(session.port, &mut session.instance.process)
                .expect("ChatGPT App page")
                .web_socket_debugger_url
        };
        let (mut socket, _) = connect(&websocket_url).expect("local theme socket");
        let conversation_started = Instant::now();
        let mut command_id = 1;
        loop {
            let entry = evaluate_value(
                &mut socket,
                command_id,
                r#"(() => {
                  const rows = [...document.querySelectorAll(
                    '[data-app-action-sidebar-thread-row], [data-app-action-sidebar-select-thread]'
                  )].filter(node => node.getBoundingClientRect().height > 0);
                  const row = rows.find(node =>
                    node.getAttribute('data-app-action-sidebar-thread-active') !== 'true'
                  );
                  if (!row) return null;
                  row.click();
                  return row.textContent?.trim() || 'history';
                })()"#,
            )
            .expect("open a history conversation");
            command_id += 1;
            if entry.is_string() {
                break;
            }
            assert!(
                conversation_started.elapsed() < Duration::from_secs(10),
                "isolated ChatGPT App has no inactive history conversation"
            );
            thread::sleep(Duration::from_millis(100));
        }
        let read_layout = |socket: &mut _, command_id| {
            evaluate_value(
                socket,
                command_id,
                r#"(() => {
                  const timeline = [...document.querySelectorAll(
                    '[data-app-action-timeline-scroll]'
                  )].find(node => node.getBoundingClientRect().height > 0);
                  const viewport = timeline?.parentElement;
                  const stage = viewport?.parentElement;
                  const header = document.querySelector(
                    '[data-ct-mount="conversation.header"]'
                  );
                  const banner = document.querySelector('[data-ct-mount="conversation.banner"]');
                  const composer = document.querySelector('[data-ct-slot="composer"]');
                  const composerBackdrop = document.querySelector(
                    '[data-ct-slot="composer.backdrop"]'
                  );
                  const contentFrame = document.querySelector(
                    '[data-ct-slot="main.content.frame"]'
                  );
                  const summaryRegion = document.querySelector(
                    '[data-ct-slot="conversation.summary.region"]'
                  );
                  const summary = document.querySelector(
                    '[data-ct-slot="conversation.summary"]'
                  );
                  const summaryDecoration = document.querySelector(
                    '[data-ct-mount="conversation.summary.decoration"]'
                  );
                  const stageRect = stage?.getBoundingClientRect();
                  const headerRect = header?.getBoundingClientRect();
                  const bannerRect = banner?.getBoundingClientRect();
                  const composerRect = composer?.getBoundingClientRect();
                  const viewportRect = viewport?.getBoundingClientRect();
                  const summaryRegionRect = summaryRegion?.getBoundingClientRect();
                  const summaryRect = summary?.getBoundingClientRect();
                  const summaryDecorationRect = summaryDecoration?.getBoundingClientRect();
                  return {
                    ready: document.documentElement.getAttribute('data-ct-view') === 'conversation'
                      && Boolean(timeline && viewport && header && banner && composer),
                    view: document.documentElement.getAttribute('data-ct-view'),
                    heroCount: document.querySelectorAll('[data-ct-mount="home.hero"]').length,
                    backgroundCount: document.querySelectorAll(
                      '[data-ct-mount="app.background"]'
                    ).length,
                    stage: header?.parentElement?.getAttribute('data-ct-slot'),
                    headerContent: banner?.parentElement?.getAttribute('data-ct-slot'),
                    viewport: viewport?.getAttribute('data-ct-slot'),
                    bannerInTimeline: Boolean(timeline?.contains(banner)),
                    headerBeforeViewport: header?.nextElementSibling === viewport,
                    headerIsFlowLayout: header
                      ? !['absolute', 'fixed'].includes(getComputedStyle(header).position)
                      : false,
                    headerClearsViewport: Boolean(headerRect && viewportRect
                      && headerRect.bottom <= viewportRect.top + 1),
                    bannerAlignedComposer: Boolean(bannerRect && composerRect
                      && Math.abs(bannerRect.left - composerRect.left) <= 2
                      && Math.abs(bannerRect.right - composerRect.right) <= 2),
                    bannerLeft: bannerRect?.left ?? null,
                    bannerRight: bannerRect?.right ?? null,
                    composerLeft: composerRect?.left ?? null,
                    composerRight: composerRect?.right ?? null,
                    viewportWidth: window.innerWidth,
                    timelinePreserved: !window.__ctHistoryTimeline
                      || window.__ctHistoryTimeline === timeline,
                    timelineScrollTop: timeline?.scrollTop ?? null,
                    contentFrameBorderTop: contentFrame
                      ? getComputedStyle(contentFrame).borderTopWidth
                      : null,
                    summaryExists: Boolean(summary),
                    summaryDecorationExists: Boolean(summaryDecoration),
                    summaryDecorationLocalUrl: summaryDecoration?.querySelector('img')?.src
                      .startsWith('http://127.0.0.1:') ?? false,
                    summaryDecorationCentered: Boolean(summaryRect && summaryDecorationRect
                      && Math.abs(
                        summaryRect.left + summaryRect.width / 2
                          - summaryDecorationRect.left - summaryDecorationRect.width / 2
                      ) <= 2),
                    summaryDecorationNonInteractive: summaryDecoration
                      ? getComputedStyle(summaryDecoration).pointerEvents === 'none'
                      : false,
                    summaryKeepsNativeOverlay: !summaryRegionRect || !headerRect
                      || summaryRegionRect.top < headerRect.bottom,
                    composerBackdropHidden: Boolean(composerBackdrop)
                      && getComputedStyle(composerBackdrop).display === 'none'
                  };
                })()"#,
            )
            .expect("history layout state")
        };
        let wait_for_layout = |socket: &mut _, command_id: &mut u64| {
            let started = Instant::now();
            loop {
                let state = read_layout(socket, *command_id);
                *command_id += 1;
                if state["ready"] == true {
                    break state;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "history layout did not stabilize: {state}"
                );
                thread::sleep(Duration::from_millis(100));
            }
        };
        let state = wait_for_layout(&mut socket, &mut command_id);
        let scroll_top = evaluate_value(
            &mut socket,
            command_id,
            r#"(() => {
              const timeline = [...document.querySelectorAll(
                '[data-app-action-timeline-scroll]'
              )].find(node => node.getBoundingClientRect().height > 0);
              if (!timeline) return null;
              timeline.scrollTop = Math.min(200, Math.max(0, timeline.scrollHeight - timeline.clientHeight));
              window.__ctHistoryTimeline = timeline;
              return timeline.scrollTop;
            })()"#,
        )
        .expect("pin history timeline and scroll position");
        command_id += 1;
        let scroll_top = scroll_top.as_f64().expect("history timeline scroll top");
        let mut responsive_states = Vec::new();
        for width in [1300, 1600, 1913] {
            set_device_metrics(&mut socket, command_id, Some(width), 1031);
            command_id += 1;
            let started = Instant::now();
            loop {
                let responsive_state = read_layout(&mut socket, command_id);
                command_id += 1;
                let settled = responsive_state["ready"] == true
                    && responsive_state["bannerAlignedComposer"] == true
                    && responsive_state["viewportWidth"].as_u64() == Some(width);
                if settled {
                    responsive_states.push(responsive_state);
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "history layout did not settle at {width}px: {responsive_state}"
                );
                thread::sleep(Duration::from_millis(50));
            }
        }
        set_device_metrics(&mut socket, command_id, None, 1031);
        command_id += 1;
        for responsive_state in &responsive_states {
            assert_eq!(
                responsive_state["timelinePreserved"], true,
                "history timeline remounted during resize: {responsive_state}"
            );
            assert!(
                responsive_state["timelineScrollTop"]
                    .as_f64()
                    .is_some_and(|value| (value - scroll_top).abs() <= 1.0),
                "history scroll position changed during resize: {responsive_state}"
            );
        }
        assert_eq!(state["heroCount"], 0, "history layout: {state}");
        assert_eq!(state["backgroundCount"], 1, "history layout: {state}");
        assert_eq!(
            state["stage"], "conversation.stage",
            "history layout: {state}"
        );
        assert_eq!(
            state["headerContent"], "conversation.header.content",
            "history layout: {state}"
        );
        assert_eq!(
            state["viewport"], "conversation.viewport",
            "history layout: {state}"
        );
        assert_eq!(state["bannerInTimeline"], false, "history layout: {state}");
        assert_eq!(
            state["headerBeforeViewport"], true,
            "history layout: {state}"
        );
        assert_eq!(state["headerIsFlowLayout"], true, "history layout: {state}");
        assert_eq!(
            state["headerClearsViewport"], true,
            "history layout: {state}"
        );
        assert_eq!(
            state["bannerAlignedComposer"], true,
            "history layout: {state}"
        );
        assert_eq!(
            state["composerBackdropHidden"], true,
            "history composer backdrop: {state}"
        );
        assert_eq!(
            state["contentFrameBorderTop"], "0px",
            "history content frame: {state}"
        );
        if state["summaryExists"] == true {
            assert_eq!(
                state["summaryDecorationExists"], true,
                "history summary: {state}"
            );
            assert_eq!(
                state["summaryDecorationLocalUrl"], true,
                "history summary: {state}"
            );
            assert_eq!(
                state["summaryDecorationCentered"], true,
                "history summary: {state}"
            );
            assert_eq!(
                state["summaryDecorationNonInteractive"], true,
                "history summary: {state}"
            );
            assert_eq!(
                state["summaryKeepsNativeOverlay"], true,
                "history summary panel: {state}"
            );
        }

        let profile_center = evaluate_value(
            &mut socket,
            command_id,
            r#"(() => {
              const footer = document.querySelector('[data-ct-slot="sidebar.footer"]');
              const buttons = [...(footer?.querySelectorAll('button') ?? [])]
                .filter(node => node.getBoundingClientRect().height > 0);
              const profile = buttons.find(node => node.textContent?.trim());
              const rect = profile?.getBoundingClientRect();
              return rect ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 } : null;
            })()"#,
        )
        .expect("profile menu control");
        let profile_x = profile_center["x"].as_f64().expect("profile menu center x");
        let profile_y = profile_center["y"].as_f64().expect("profile menu center y");
        command_id += 1;
        dispatch_mouse_click(&mut socket, command_id, profile_x, profile_y);
        command_id += 2;
        let menu_started = Instant::now();
        let settings_center = loop {
            let center = evaluate_value(
                &mut socket,
                command_id,
                r#"(() => {
                  const popper = [...document.querySelectorAll(
                    '[data-radix-popper-content-wrapper]'
                  )].find(node => node.getBoundingClientRect().height > 0);
                  const items = [...(popper?.querySelectorAll(
                    '[data-radix-collection-item], [role="menuitem"]'
                  ) ?? [])].filter(node => node.getBoundingClientRect().height > 0);
                  const settings = items.at(-1);
                  const rect = settings?.getBoundingClientRect();
                  return rect
                    ? { x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }
                    : null;
                })()"#,
            )
            .expect("settings menu item");
            command_id += 1;
            if center.is_object() {
                break center;
            }
            assert!(
                menu_started.elapsed() < Duration::from_secs(5),
                "profile menu did not expose settings"
            );
            thread::sleep(Duration::from_millis(100));
        };
        dispatch_mouse_click(
            &mut socket,
            command_id,
            settings_center["x"].as_f64().expect("settings center x"),
            settings_center["y"].as_f64().expect("settings center y"),
        );
        command_id += 2;
        let settings_started = Instant::now();
        loop {
            let settings_ready = evaluate(
                &mut socket,
                command_id,
                "Boolean(document.querySelector('[data-settings-panel-slug]'))",
            )
            .expect("settings state");
            command_id += 1;
            if settings_ready {
                break;
            }
            assert!(settings_started.elapsed() < Duration::from_secs(10));
            thread::sleep(Duration::from_millis(100));
        }
        dispatch_escape(&mut socket, command_id);
        command_id += 2;
        let returned_state = wait_for_layout(&mut socket, &mut command_id);
        assert_eq!(
            returned_state["heroCount"], 0,
            "returned layout: {returned_state}"
        );
        assert_eq!(
            returned_state["backgroundCount"], 1,
            "returned layout: {returned_state}"
        );
        assert_eq!(
            returned_state["bannerInTimeline"], false,
            "returned layout: {returned_state}"
        );
        assert_eq!(
            returned_state["headerBeforeViewport"], true,
            "returned layout: {returned_state}"
        );
        let _ = socket.close(None);
        assert!(stop_theme_preview(&runtime).expect("theme preview should stop"));
    }
}
