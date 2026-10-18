// 발급한 코드로 초대하거나 다른 기기의 코드를 입력해 연결한다.
window.peerConnections = (() => {
  let invitation = null;
  const element = id => document.getElementById(id);
  function init() {
    const app = window.libraryApp;
    element("invite-form").addEventListener("submit", event => {
      event.preventDefault(); app.perform(event.currentTarget.querySelector('[type="submit"]'), async () => {
        const id = element("invite-library").value;
        const result = await app.manage("invite", { id });
        invitation = { ...result, id, until: Date.now() + result.expiresIn * 1000 };
        element("invite-code").value = result.code;
        element("invite-qr").src = `data:image/svg+xml;base64,${btoa(result.qr_svg)}`;
        element("invite-notice").textContent = ""; element("invite-result").hidden = false; tick();
      });
    });
    element("invite-copy").addEventListener("click", () => app.copy(element("invite-code").value));
    element("invite-cancel").addEventListener("click", () => app.perform(element("invite-cancel"), async () => {
      if (invitation) await app.manage("cancelInvite", { id: invitation.id }); invitation = null; element("invite-result").hidden = true; element("invite-notice").textContent = "연결 코드를 취소했습니다.";
    }));
    element("connect-form").addEventListener("submit", event => {
      event.preventDefault(); const form = event.currentTarget;
      app.perform(form.querySelector('[type="submit"]'), async () => {
        element("connect-notice").textContent = "상대 기기에 연결하고 있습니다…";
        await app.manage("pair", { id: element("connect-library").value, code: element("connect-code").value.trim() });
        element("connect-code").value = ""; element("connect-notice").textContent = "기기를 연결했습니다. 동기화를 시작합니다."; app.navigate("devices"); app.toast("기기를 연결했습니다.");
      });
    });
    setInterval(tick, 1000);
  }
  function tick() {
    if (!invitation) return;
    const seconds = Math.max(0, Math.ceil((invitation.until - Date.now()) / 1000));
    element("invite-expiry").textContent = `한 번만 사용 가능 · ${Math.floor(seconds / 60)}분 ${seconds % 60}초 뒤 만료`;
    if (!seconds) { invitation = null; element("invite-result").hidden = true; element("invite-notice").textContent = "연결 코드가 만료되었습니다. 새 코드를 만들어 주세요."; }
  }
  function refresh(status) {
    const ready = status?.state === "running";
    for (const id of ["invite-form", "connect-form"]) for (const input of element(id).querySelectorAll("button,input,select,textarea")) input.disabled = !ready || window.libraryApp.busy;
    if (invitation && status.peer?.pendingInvites && !status.peer.pendingInvites.includes(invitation.id)) {
      invitation = null; element("invite-result").hidden = true; element("invite-notice").textContent = "기기가 연결되었습니다. 연결된 기기 메뉴에서 확인할 수 있습니다.";
    }
  }
  return { init, refresh };
})();
