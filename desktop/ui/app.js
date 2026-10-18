const { invoke } = window.__TAURI__.core;
const $ = id => document.getElementById(id);
let status = null, page = "overview", busy = false, refreshing = null, toastTimer;
let nameAction = null, confirmAction = null, previousFocus = null, resetShown = false;
const manage = (action, args = {}) => invoke("realtime_manage", { action, args });
function toast(message, error = false) {
  $("toast").textContent = String(message); $("toast").classList.toggle("error", error); $("toast").hidden = false;
  clearTimeout(toastTimer); toastTimer = setTimeout(() => { $("toast").hidden = true; }, 4500);
}
function navigate(next) {
  page = next;
  for (const button of document.querySelectorAll("[data-page]")) button.setAttribute("aria-pressed", String(button.dataset.page === next));
  for (const section of document.querySelectorAll("main > .page")) section.hidden = section.id !== `page-${next}`;
}
function bytes(value) {
  if (value >= 1024 ** 3) return `${(value / 1024 ** 3).toFixed(1)} GB`;
  if (value >= 1024 ** 2) return `${(value / 1024 ** 2).toFixed(1)} MB`;
  if (value >= 1024) return `${Math.round(value / 1024)} KB`;
  return `${value} B`;
}
function closeModal(id) { $(id).hidden = true; if (previousFocus?.isConnected) previousFocus.focus(); }
function nameDialog(title, value, action) {
  previousFocus = document.activeElement; nameAction = action;
  $("name-title").textContent = title; $("name-value").value = value; $("name-modal").hidden = false;
  $("name-value").maxLength = title.includes("기기") ? 100 : 200; $("name-value").focus();
}
function confirm(title, message, ok = "연결 해제") {
  return new Promise(resolve => { previousFocus = document.activeElement; confirmAction = resolve;
    $("confirm-title").textContent = title; $("confirm-message").textContent = message; $("confirm-ok").textContent = ok; $("confirm-modal").hidden = false; $("confirm-cancel").focus();
  });
}
async function perform(button, action) {
  if (busy) return; busy = true; if (button) button.disabled = true;
  peerConnections.refresh(status);
  try { await action(); } catch (error) { toast(typeof error === "string" ? error : String(error), true); $("connect-notice").textContent = ""; }
  finally { busy = false; if (button?.isConnected) button.disabled = false; await refresh(); }
}
async function copy(text) { try { await navigator.clipboard.writeText(text); toast("복사했습니다."); } catch { toast("복사하지 못했습니다.", true); } }
async function refresh() {
  if (refreshing) return refreshing;
  refreshing = (async () => {
  try {
    status = await invoke("server_status");
    const active = ["starting", "running"].includes(status.state), peer = status.peer;
    const labels = { starting: "동기화 시작 중", running: "동기화 실행 중", stopped: "동기화 중지됨", error: "실행 오류" };
    $("state-label").textContent = labels[status.state] || "확인 중";
    $("state-dot").className = `dot ${status.state === "running" ? "running" : status.state === "error" ? "error" : ""}`;
    $("toggle-server").textContent = active ? "중지" : "시작"; $("toggle-server").disabled = busy;
    $("restart").hidden = !active; $("restart").disabled = busy;
    $("version").textContent = `v${status.version}`;
    $("data-dir").textContent = status.data_dir;
    if (document.activeElement !== $("settings-autostart")) $("settings-autostart").checked = status.autostart;
    $("error-callout").hidden = status.state !== "error" && !peer?.error;
    $("error-message").textContent = status.message || peer?.error || "";
    const libraries = peer?.libraries || [];
    const peers = libraryViews.render(libraries, status.state === "running");
    $("stat-libraries").textContent = libraries.length;
    $("stat-files").textContent = libraries.reduce((sum, library) => sum + (library.statistics?.files || 0), 0);
    $("stat-storage").textContent = bytes(libraries.reduce((sum, library) => sum + (library.statistics?.bytes || 0), 0));
    $("stat-online").textContent = new Set(peers.filter(peer => peer.online && active).map(peer => peer.id)).size;
    for (const button of document.querySelectorAll('[data-action="create-library"]')) button.disabled = !active || busy;
    peerConnections.refresh(status);
    // 이전 1:1 기기 연결을 초기화했다면 한 번 안내한다.
    if (peer?.resetNotice && !resetShown) {
      resetShown = true; previousFocus = document.activeElement; $("reset-modal").hidden = false; $("reset-ok").focus();
    }
  } catch (error) { toast(String(error), true); }
  })().finally(() => { refreshing = null; });
  return refreshing;
}
async function refreshUpdate() {
  try {
    const update = await invoke("update_status"); const phase = update.phase;
    $("update-version").textContent = update.version ? `v${update.current_version} → v${update.version}` : `현재 버전 v${update.current_version}`;
    $("update-message").textContent = update.message || ({ idle: "새 버전을 자동으로 확인합니다.", checking: "업데이트 확인 중…", available: "업데이트할 수 있습니다.", latest: "최신 버전입니다.", downloading: "업데이트 다운로드 중…", installing: "업데이트 설치 중…" }[phase] || "");
    $("update-check").disabled = ["checking", "downloading", "installing"].includes(phase);
    $("update-install").hidden = !update.version; $("update-install").disabled = phase !== "available";
  } catch (error) { toast(String(error), true); }
}
window.libraryApp = { invoke, manage, navigate, toast, bytes, nameDialog, confirm, perform, copy, get busy() { return busy; } };
for (const button of document.querySelectorAll("[data-page], [data-go]")) button.addEventListener("click", () => navigate(button.dataset.page || button.dataset.go));
for (const button of document.querySelectorAll('[data-action="create-library"]')) button.addEventListener("click", () => nameDialog("서재 만들기", "", async name => {
  const library = await manage("create", { name });
  await refresh(); for (const id of ["invite-library", "connect-library"]) $(id).value = library.id;
  if (["overview", "libraries"].includes(page)) navigate("libraries");
  toast("서재를 만들었습니다.");
}));
for (const button of document.querySelectorAll('[data-action="open-data"], [data-action="open-log"]')) button.addEventListener("click", () => perform(button, () => invoke(button.dataset.action === "open-data" ? "open_data_dir" : "open_log")));
$("toggle-server").addEventListener("click", event => perform(event.currentTarget, () => invoke(["running", "starting"].includes(status?.state) ? "server_stop" : "server_start")));
$("restart").addEventListener("click", event => perform(event.currentTarget, () => invoke("server_restart")));
$("settings-autostart").addEventListener("change", event => perform(event.currentTarget, () => invoke("settings_save", { autostart: event.target.checked })));
$("update-check").addEventListener("click", event => perform(event.currentTarget, async () => { await invoke("update_check"); await refreshUpdate(); }));
$("update-install").addEventListener("click", event => perform(event.currentTarget, () => invoke("update_install")));
$("name-form").addEventListener("submit", event => {
  event.preventDefault(); const action = nameAction, name = $("name-value").value.trim();
  perform(event.currentTarget.querySelector('[type="submit"]'), async () => { await action(name); closeModal("name-modal"); });
});
for (const button of document.querySelectorAll("[data-close]")) button.addEventListener("click", () => closeModal(button.dataset.close));
function finishConfirm(answer) { closeModal("confirm-modal"); const resolve = confirmAction; confirmAction = null; resolve?.(answer); }
$("confirm-cancel").addEventListener("click", () => finishConfirm(false)); $("confirm-ok").addEventListener("click", () => finishConfirm(true));
$("reset-ok").addEventListener("click", () => { closeModal("reset-modal"); manage("dismissReset").catch(error => toast(String(error), true)); });
document.addEventListener("keydown", event => {
  const modal = [...document.querySelectorAll('.backdrop:not([hidden])')].at(-1);
  if (!modal) return;
  if (event.key === "Escape") { event.preventDefault(); modal.id === "confirm-modal" ? finishConfirm(false) : modal.id === "reset-modal" ? $("reset-ok").click() : closeModal(modal.id); }
  if (event.key === "Tab") { const focus = [...modal.querySelectorAll('button:not(:disabled),input:not(:disabled),textarea:not(:disabled),select:not(:disabled)')]; const index = focus.indexOf(document.activeElement); if (focus.length && (event.shiftKey ? index <= 0 : index === focus.length - 1)) { event.preventDefault(); focus[event.shiftKey ? focus.length - 1 : 0].focus(); } }
});
peerConnections.init(); refresh(); refreshUpdate(); setInterval(refresh, 2000); setInterval(refreshUpdate, 3000);
