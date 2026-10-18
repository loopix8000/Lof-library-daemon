// 동기화 서재와 서재별 그룹 구성원을 표시한다.
window.libraryViews = (() => {
  const node = (tag, cls, text) => { const value = document.createElement(tag); if (cls) value.className = cls; if (text !== undefined) value.textContent = text; return value; };
  const button = (label, action, danger = false) => { const value = node("button", `small${danger ? " danger-outline" : ""}`, label); value.type = "button"; value.addEventListener("click", () => window.libraryApp.perform(value, action)); return value; };
  // 웹 관리 화면에서는 숨기는 기능(서버 PC의 폴더 열기 등)이다.
  const desktopOnly = value => { value.dataset.desktopOnly = ""; return value; };
  let lastLibraries = "", lastPeers = "";
  function render(libraries, online) {
    const app = window.libraryApp;
    const current = JSON.stringify([libraries, online]);
    if (current !== lastLibraries) {
      lastLibraries = current;
      document.getElementById("libraries").replaceChildren(...libraries.map(library => {
        const row = node("li", "library-card"); const details = node("div", "grow"); const stats = library.statistics || {};
        const group = library.group ? `그룹 ${library.group.id.slice(0, 8).toUpperCase().replace(/(.{4})/, "$1-")} · 기기 ${library.peers.length}대` : "그룹 없음";
        details.append(node("strong", "title", library.name), node("span", "meta", `${stats.files ?? 0}개 파일 · ${app.bytes(stats.bytes || 0)} · ${group}${library.syncing ? " · 동기화 중" : ""}`), node("span", "path", library.path));
        if (stats.error) details.append(node("span", "error-text", stats.error));
        const actions = node("div", "actions");
        actions.append(button("기기 초대", async () => { app.navigate("invite"); document.getElementById("invite-library").value = library.id; }),desktopOnly(button("폴더 열기", () => app.invoke("open_library", { id: library.id }))),button("이름 변경", async () => { app.nameDialog("서재 이름 변경", library.name, name => app.manage("rename", { id: library.id, name })); }));
        if (library.group) actions.append(button("그룹 나가기", async () => {
          if (await app.confirm("동기화 그룹에서 나갈까요?", `‘${library.name}’ 서재가 그룹의 다른 기기와 더 이상 동기화하지 않습니다. 원본 파일은 이 Library에 그대로 남습니다.`, "그룹 나가기")) await app.manage("leave", { id: library.id });
        }, true));
        for (const child of actions.children) child.disabled = !online;
        row.append(details, actions); return row;
      }));
      document.getElementById("libraries-empty").hidden = libraries.length > 0;
    }
    const peers = libraries.flatMap(library => library.peers.map(peer => ({ ...peer, vault: library.id, library: library.name, lastSync: library.statistics?.lastSync })));
    const peerVersion = JSON.stringify([peers, online]);
    if (peerVersion !== lastPeers) {
      lastPeers = peerVersion;
      document.getElementById("devices").replaceChildren(...peers.map(peer => {
        const row = node("li"); const info = node("div", "grow");
        info.append(node("strong", "title", peer.name),node("span", "meta", `${peer.library} · ${online && peer.online ? "연결됨" : "오프라인"}${peer.storage ? " · Lof Library" : " · Writer"}`));
        if (peer.status?.message) info.append(node("span", "error-text", peer.status.message));
        if (peer.lastSync) info.append(node("span", "meta", `최근 동기화 ${new Date(peer.lastSync * 1000).toLocaleString()}`));
        const actions = node("div", "actions");
        actions.append(button("이름 변경", async () => { app.nameDialog("기기 이름 변경",peer.name,name => app.manage("renamePeer",{vault:peer.vault,peer:peer.id,name})); }),button("그룹에서 제거",async () => {
          if (await app.confirm("이 기기를 그룹에서 제거할까요?",`${peer.name}을(를) ‘${peer.library}’ 서재의 동기화 그룹에서 제거합니다. 그룹의 다른 기기에도 함께 반영됩니다.`,"제거")) await app.manage("remove",{vault:peer.vault,peer:peer.id});
        },true));
        for (const child of actions.children) child.disabled = !online;
        row.append(node("span", online && peer.online ? "dot running" : "dot"),info,actions); return row;
      }));
      document.getElementById("devices-empty").hidden = peers.length > 0;
    }
    for (const id of ["invite-library", "connect-library"]) {
      const select = document.getElementById(id); const saved = select.value;
      if (select.dataset.options !== JSON.stringify(libraries.map(library => [library.id, library.name]))) {
        select.replaceChildren(...libraries.map(library => { const option = node("option", "", library.name); option.value = library.id; return option; }));
        if (libraries.some(library => library.id === saved)) select.value = saved;
        select.dataset.options = JSON.stringify(libraries.map(library => [library.id, library.name]));
      }
    }
    return peers;
  }
  return { render };
})();
