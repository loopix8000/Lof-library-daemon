// 데스크톱 관리 화면이 쓰는 Tauri 명령을 서버의 HTTP 관리 API로 연결한다.
(() => {
  document.documentElement.dataset.web = "1";
  const style = document.createElement("style");
  style.textContent = 'html[data-web] [data-desktop-only]{display:none!important}.web-logout{margin-top:auto}';
  document.head.append(style);
  let version = "";
  async function api(path, body) {
    const options = body === undefined
      ? { credentials: "same-origin" }
      : { method: "POST", credentials: "same-origin", headers: { "content-type": "application/json" }, body: JSON.stringify(body) };
    const response = await fetch(path, options);
    if (response.status === 401) { location.href = "/login"; throw "로그인이 필요합니다."; }
    const data = await response.json().catch(() => ({}));
    if (!response.ok) throw data.error || `요청 실패 (${response.status})`;
    return data;
  }
  const unsupported = () => { throw "웹 관리 화면에서는 지원하지 않는 기능입니다."; };
  const commands = {
    server_status: async () => { const status = await api("/api/status"); version = status.version; return status; },
    realtime_manage: ({ action, args }) => api("/api/manage", { action, args }),
    update_status: async () => ({ phase: "idle", current_version: version }),
    settings_save: async () => {},
  };
  window.__TAURI__ = { core: { invoke: (command, args) => (commands[command] || unsupported)(args) } };
  window.addEventListener("DOMContentLoaded", () => {
    const logout = document.createElement("button");
    logout.type = "button"; logout.className = "small ghost web-logout"; logout.textContent = "로그아웃";
    logout.addEventListener("click", async () => { try { await api("/api/logout", {}); } finally { location.href = "/login"; } });
    document.querySelector(".sidebar")?.append(logout);
  });
})();
