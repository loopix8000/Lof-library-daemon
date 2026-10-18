const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const ADDRESS_KINDS = {
  lan: { tag: "같은 네트워크", note: "같은 공유기에 연결된 기기에서 사용합니다." },
  vpn: { tag: "VPN", note: "Tailscale 같은 VPN으로 연결된 기기에서 사용합니다." },
  public: { tag: "인터넷", note: "외부에서 접속하려면 공유기에서 이 PC로 포트 포워딩을 설정해야 합니다." },
  local: { tag: "이 PC", note: "이 PC의 Lof Writer에서 사용합니다." },
};

let status = null;
let devices = [];
let page = "overview";
let editingDevice = null;
let pairing = null;
let pairingTimer = null;
let toastTimer = null;
let settingsLoaded = false;
let updateStatus = null;

// 공통

function toast(message, error = false) {
  const element = $("toast");
  element.textContent = message;
  element.classList.toggle("error", error);
  element.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { element.hidden = true; }, 3500);
}

function failure(error) {
  toast(typeof error === "string" ? error : "작업을 완료하지 못했습니다.", true);
}

async function run(button, action) {
  if (button) button.disabled = true;
  try {
    await action();
  } catch (error) {
    failure(error);
  } finally {
    if (button) button.disabled = false;
    await refresh();
  }
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(label, className, onClick) {
  const node = element("button", className, label);
  node.type = "button";
  node.addEventListener("click", onClick);
  return node;
}

function icon(name) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  const use = document.createElementNS("http://www.w3.org/2000/svg", "use");
  use.setAttribute("href", `#i-${name}`);
  svg.append(use);
  return svg;
}

function duration(seconds) {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days) return `${days}일 ${hours}시간`;
  if (hours) return `${hours}시간 ${minutes}분`;
  return `${Math.max(1, minutes)}분`;
}

function bytes(value) {
  if (value >= 1024 ** 3) return `${(value / 1024 ** 3).toFixed(1)} GB`;
  if (value >= 1024 ** 2) return `${(value / 1024 ** 2).toFixed(1)} MB`;
  return `${Math.max(1, Math.round(value / 1024))} KB`;
}

function seen(timestamp) {
  if (!timestamp) return "접속 기록 없음";
  const elapsed = Date.now() / 1000 - timestamp;
  if (elapsed < 120) return "방금 전 접속";
  if (elapsed < 3600) return `${Math.floor(elapsed / 60)}분 전 접속`;
  if (elapsed < 86400) return `${Math.floor(elapsed / 3600)}시간 전 접속`;
  return `${new Date(timestamp * 1000).toLocaleDateString("ko-KR")} 접속`;
}

function copy(value, message) {
  navigator.clipboard.writeText(value).then(() => toast(message), () => toast("복사하지 못했습니다.", true));
}

function openModal(id) {
  $(id).hidden = false;
  const focus = $(id).querySelector("input:not([type=radio]), select, .primary");
  if (focus) setTimeout(() => focus.focus(), 0);
}

function closeModal(id) {
  $(id).hidden = true;
}

function confirmDialog(title, message, label) {
  $("confirm-title").textContent = title;
  $("confirm-message").textContent = message;
  $("confirm-ok").textContent = label;
  $("confirm-modal").hidden = false;
  $("confirm-cancel").focus();
  return new Promise((resolve) => {
    const finish = (value) => {
      $("confirm-modal").hidden = true;
      $("confirm-ok").onclick = null;
      $("confirm-cancel").onclick = null;
      resolve(value);
    };
    $("confirm-ok").onclick = () => finish(true);
    $("confirm-cancel").onclick = () => finish(false);
  });
}

// 화면

function showPage(name) {
  page = name;
  for (const item of document.querySelectorAll(".nav button")) {
    item.setAttribute("aria-pressed", String(item.dataset.page === name));
  }
  for (const section of document.querySelectorAll(".page")) {
    section.hidden = section.id !== `page-${name}`;
  }
  $("page-" + name).parentElement.scrollTop = 0;
}

function renderSidebar() {
  const running = status.state === "running";
  const server = status.server;
  $("state-dot").className = `dot ${status.state}`;
  $("state-label").textContent = { running: "실행 중", stopped: "중지됨", error: "실행 오류" }[status.state];
  $("state-detail").textContent = running && server
    ? `${duration(server.uptime_seconds)}째 실행 중`
    : status.state === "error" ? "개요에서 원인을 확인해 주세요." : "동기화를 사용할 수 없습니다.";
  const toggle = $("toggle-server");
  toggle.textContent = running ? "중지" : "시작";
  toggle.classList.toggle("primary", !running);
  $("restart").hidden = !running;
  $("version").textContent = server ? `${server.name} · v${server.version}` : "";
  const count = $("device-count");
  count.hidden = !running || devices.length === 0;
  count.textContent = String(devices.length);
}

function renderAddresses() {
  const items = status.addresses.map((address) => {
    const kind = ADDRESS_KINDS[address.kind];
    const item = element("li");
    const text = element("div", "grow");
    const title = element("span", "title mono", address.url);
    text.append(title, element("span", "meta", kind.note));
    const actions = element("div", "actions");
    actions.append(element("span", "tag", kind.tag));
    const copyButton = button("", "icon", () => copy(address.url, "접속 주소를 복사했습니다."));
    copyButton.title = "복사";
    copyButton.setAttribute("aria-label", `${address.url} 복사`);
    copyButton.append(icon("copy"));
    actions.append(copyButton);
    item.append(text, actions);
    return item;
  });
  if (status.settings.allow_lan && !status.addresses.some((address) => address.kind === "public")) {
    const item = element("li");
    const text = element("div", "grow");
    const checking = status.public_ip === "checking";
    text.append(
      element("span", "title", checking ? "공인 IP 확인 중…" : "공인 IP를 확인하지 못했습니다"),
      element("span", "meta", checking ? "인터넷에서 접속할 주소를 확인하고 있습니다." : "인터넷 연결을 확인한 뒤 다시 시도해 주세요."),
    );
    item.append(text);
    if (!checking) {
      const actions = element("div", "actions");
      actions.append(button("다시 확인", "small", (event) => run(event.currentTarget, () => invoke("public_ip_refresh"))));
      item.append(actions);
    }
    items.splice(items.length - 1, 0, item);
  }
  $("addresses").replaceChildren(...items);
}

function renderOverview() {
  const running = status.state === "running";
  const server = status.server;
  $("error-callout").hidden = status.state !== "error";
  $("error-message").textContent = status.message || "";
  $("stat-libraries").textContent = server ? `${server.counts.libraries}개` : "-";
  $("stat-documents").textContent = server ? `${server.counts.documents}개` : "-";
  $("stat-online").textContent = server ? `${server.connected_devices}대` : "-";
  $("stat-storage").textContent = server ? bytes(server.storage_bytes) : "-";
  for (const add of document.querySelectorAll('[data-action="add-device"]')) add.disabled = !running;
  renderAddresses();
}

function renderSettings() {
  if (!settingsLoaded) {
    $("settings-port").value = status.settings.port;
    $("settings-lan").checked = status.settings.allow_lan;
    $("settings-autostart").checked = status.autostart;
    settingsLoaded = true;
  }
  $("port-apply").disabled = Number($("settings-port").value) === status.settings.port;
  $("data-dir").textContent = status.data_dir;
}

function renderUpdate() {
  if (!updateStatus) return;
  const { phase, current_version: current, version, progress, message } = updateStatus;
  const busy = ["checking", "downloading", "installing"].includes(phase);
  $("update-version").textContent = version ? `v${current} → v${version}` : `현재 버전 v${current}`;
  $("update-message").textContent = message || {
    idle: "새 버전을 자동으로 확인합니다.",
    checking: "새 버전을 확인하고 있습니다…",
    available: "새 버전을 설치할 수 있습니다.",
    latest: "최신 버전입니다.",
    downloading: progress === null ? "업데이트를 내려받고 있습니다…" : `업데이트 다운로드 중 ${progress}%`,
    installing: "서버를 종료하고 업데이트를 설치합니다…",
  }[phase] || "잠시 후 다시 확인해 주세요.";
  $("update-check").disabled = busy;
  $("update-install").hidden = !version;
  $("update-install").disabled = busy;
}

async function refreshUpdate() {
  updateStatus = await invoke("update_status");
  renderUpdate();
}

function renderDevices() {
  const running = status.state === "running";
  $("devices-empty").hidden = !running || devices.length > 0;
  $("devices").replaceChildren(...devices.map((device) => {
    const item = element("li");
    const dot = element("span", device.connected ? "dot running" : "dot");
    const text = element("div", "grow");
    const actions = element("div", "actions");
    if (editingDevice === device.id) {
      const input = element("input");
      input.value = device.name;
      input.maxLength = 100;
      const save = () => run(null, async () => {
        await invoke("device_rename", { id: device.id, name: input.value });
        editingDevice = null;
        toast("기기 이름을 바꿨습니다.");
      });
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter") save();
        if (event.key === "Escape") { editingDevice = null; renderDevices(); }
      });
      item.append(dot, input);
      actions.append(
        button("취소", "small", () => { editingDevice = null; renderDevices(); }),
        button("저장", "small primary", save),
      );
      setTimeout(() => input.focus(), 0);
    } else {
      const platform = device.platform ? `${device.platform} · ` : "";
      text.append(
        element("span", "title", device.name),
        element("span", "meta", `${platform}${device.connected ? "실시간 연결 중" : seen(device.last_seen_at)}`),
      );
      item.append(dot, text);
      actions.append(
        button("이름 변경", "small", () => { editingDevice = device.id; renderDevices(); }),
        button("연결 해제", "small danger-outline", async (event) => {
          const target = event.currentTarget;
          const ok = await confirmDialog(
            "연결을 해제할까요?",
            `‘${device.name}’은(는) 더 이상 동기화할 수 없습니다. 다시 사용하려면 연결 코드로 새로 연결해야 합니다.`,
            "연결 해제",
          );
          if (!ok) return;
          run(target, async () => {
            await invoke("device_revoke", { id: device.id });
            toast("연결을 해제했습니다.");
          });
        }),
      );
    }
    item.append(actions);
    return item;
  }));
}

function renderPairingAddresses() {
  const select = $("pairing-url");
  const urls = status.addresses.map((address) => address.url);
  const current = [...select.options].map((option) => option.value).join("\n");
  if (current !== urls.join("\n")) {
    const selected = select.value;
    select.replaceChildren(...status.addresses.map((address) =>
      new Option(`${address.url}  (${ADDRESS_KINDS[address.kind].tag})`, address.url)));
    if (urls.includes(selected)) select.value = selected;
  }
  $("pairing-local-hint").hidden = status.settings.allow_lan;
}

function renderPairing() {
  clearInterval(pairingTimer);
  const stage = !pairing ? "loading" : pairing.done ? "done" : "result";
  $("pairing-loading").hidden = stage !== "loading";
  $("pairing-result").hidden = stage !== "result";
  $("pairing-done").hidden = stage !== "done";
  if (stage === "done") {
    $("pairing-done-title").textContent = `‘${pairing.done}’을(를) 연결했습니다`;
    return;
  }
  if (stage !== "result") return;
  $("pairing-code").textContent = pairing.entry.code;
  $("pairing-qr").innerHTML = pairing.entry.qr_svg;
  const expiry = $("pairing-expiry");
  const tick = () => {
    const left = Math.round(pairing.entry.expires_at - Date.now() / 1000);
    expiry.classList.toggle("expired", left <= 0);
    if (left <= 0) {
      expiry.textContent = "유효 시간이 지났습니다. 창을 닫고 새 코드를 발급해 주세요.";
      clearInterval(pairingTimer);
      return;
    }
    expiry.textContent = `남은 시간 ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")} · 한 번만 사용`;
  };
  tick();
  pairingTimer = setInterval(tick, 1000);
}

function checkPairingDone() {
  if (!pairing || pairing.done) return;
  const added = devices.find((device) => !pairing.known.has(device.id));
  if (added) {
    pairing.done = added.name;
    renderPairing();
  }
}

async function refresh() {
  try {
    status = await invoke("server_status");
    if (status.state === "running") {
      if (editingDevice === null) devices = (await invoke("devices_list")).devices;
    } else {
      devices = [];
      editingDevice = null;
    }
    renderSidebar();
    renderOverview();
    renderSettings();
    if (editingDevice === null) renderDevices();
    renderPairingAddresses();
    checkPairingDone();
    await libraryManager.refresh(status.state === "running");
  } catch (error) {
    failure(error);
  }
}

// 이벤트

$("update-check").addEventListener("click", async () => {
  $("update-check").disabled = true;
  try {
    await invoke("update_check");
  } catch (error) {
    failure(error);
  } finally {
    await refreshUpdate().catch(failure);
  }
});

$("update-install").addEventListener("click", async () => {
  const ok = await confirmDialog("업데이트를 설치할까요?", "설치하는 동안 서버가 잠시 종료됩니다. 설치 후 다시 시작하면 기존 데이터와 기기 연결을 이어서 사용합니다.", "설치하고 다시 시작");
  if (!ok) return;
  $("update-install").disabled = true;
  try {
    await invoke("update_install");
  } catch (error) {
    failure(error);
  } finally {
    await refreshUpdate().catch(failure);
  }
});

for (const item of document.querySelectorAll(".nav button")) {
  item.addEventListener("click", () => showPage(item.dataset.page));
}

$("toggle-server").addEventListener("click", (event) => run(event.currentTarget, async () => {
  if (status?.state === "running") {
    const ok = await confirmDialog("서버를 중지할까요?", "중지하는 동안 연결된 기기는 동기화할 수 없습니다.", "중지");
    if (ok) await invoke("server_stop");
  } else {
    await invoke("server_start");
  }
}));
$("restart").addEventListener("click", (event) => run(event.currentTarget, async () => {
  await invoke("server_restart");
  toast("서버를 다시 시작했습니다.");
}));

document.addEventListener("click", (event) => {
  const target = event.target.closest("[data-action]");
  if (!target) return;
  const action = target.dataset.action;
  if (action === "add-device") {
    startPairing();
  } else if (action === "close-pairing") {
    closeModal("pairing-modal");
    clearInterval(pairingTimer);
    pairing = null;
  } else if (action === "cancel-pairing" && pairing) {
    run(target, async () => {
      await invoke("pairing_cancel", { id: pairing.entry.id });
      closeModal("pairing-modal");
      pairing = null;
      toast("연결 코드를 취소했습니다.");
    });
  } else if (action === "open-data") {
    invoke("open_data_dir").catch(failure);
  } else if (action === "open-log") {
    invoke("open_log").catch(failure);
  }
});

document.addEventListener("keydown", (event) => {
  if (event.key !== "Escape") return;
  if (!$("confirm-modal").hidden) $("confirm-cancel").click();
  else if (libraryManager.closeOnEscape()) return;
  else if (!$("pairing-modal").hidden) document.querySelector('#pairing-modal [data-action="close-pairing"]').click();
});

// 기기 추가를 누르면 맨 앞 접속 주소로 바로 코드를 발급해 보여 준다.
async function startPairing() {
  pairing = null;
  renderPairing();
  const select = $("pairing-url");
  if (status?.addresses?.length) select.value = status.addresses[0].url;
  openModal("pairing-modal");
  try {
    const known = new Set(devices.map((device) => device.id));
    const entry = await invoke("pairing_create", { serverUrl: select.value });
    // 발급 중에 창을 닫았으면 쓰지 않을 코드를 바로 취소한다.
    if ($("pairing-modal").hidden) {
      invoke("pairing_cancel", { id: entry.id }).catch(() => {});
      return;
    }
    pairing = { entry, known, done: null };
    renderPairing();
  } catch (error) {
    closeModal("pairing-modal");
    failure(error);
  }
}

// 접속 주소를 바꾸면 같은 코드로 연결 정보와 QR만 다시 만든다.
$("pairing-url").addEventListener("change", (event) => {
  if (!pairing || pairing.done) return;
  invoke("pairing_link", { code: pairing.entry.code, serverUrl: event.target.value })
    .then((link) => {
      Object.assign(pairing.entry, link);
      $("pairing-qr").innerHTML = link.qr_svg;
    })
    .catch(failure);
});

$("pairing-copy").addEventListener("click", () => {
  if (pairing) copy(pairing.entry.connection, "연결 정보를 복사했습니다.");
});

async function saveSettings(message) {
  try {
    await invoke("settings_save", {
      port: Number($("settings-port").value),
      allowLan: $("settings-lan").checked,
      autostart: $("settings-autostart").checked,
    });
    toast(message);
  } finally {
    // 저장에 실패해도 화면을 실제 설정으로 되돌린다.
    settingsLoaded = false;
  }
}

$("settings-port").addEventListener("input", () => {
  $("port-apply").disabled = !status || Number($("settings-port").value) === status.settings.port;
});
$("port-form").addEventListener("submit", (event) => {
  event.preventDefault();
  run($("port-apply"), () => saveSettings("포트를 바꾸고 서버를 다시 시작했습니다."));
});
$("settings-lan").addEventListener("change", (event) => {
  const allowed = event.target.checked;
  run(null, () => saveSettings(allowed ? "다른 기기에서 접속할 수 있습니다." : "이 PC에서만 접속할 수 있습니다."));
});
$("settings-autostart").addEventListener("change", (event) => {
  const enabled = event.target.checked;
  run(null, () => saveSettings(enabled ? "PC를 시작하면 자동으로 실행합니다." : "자동 실행을 껐습니다."));
});

libraryManager.init();
refresh();
setInterval(refresh, 3000);
refreshUpdate().catch(failure);
setInterval(() => refreshUpdate().catch(failure), 1000);
