const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
let latest = null;
let settingsLoaded = false;
let editingDevice = null;
let pairing = null;
let pairingTimer = null;
let toastTimer = null;

function toast(message) {
  const element = $("toast");
  element.textContent = message;
  element.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { element.hidden = true; }, 3500);
}

function failure(error) {
  toast(typeof error === "string" ? error : "작업을 완료하지 못했습니다.");
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

function duration(seconds) {
  const days = Math.floor(seconds / 86400);
  const hours = Math.floor((seconds % 86400) / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  if (days) return `${days}일 ${hours}시간`;
  if (hours) return `${hours}시간 ${minutes}분`;
  return `${minutes}분`;
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
  navigator.clipboard.writeText(value).then(() => toast(message), () => toast("복사하지 못했습니다."));
}

function renderStatus(status) {
  const running = status.state === "running";
  const pill = $("status-pill");
  pill.className = `pill ${status.state}`;
  pill.textContent = { running: "실행 중", stopped: "중지됨", error: "오류" }[status.state];
  $("start").hidden = running;
  $("restart").hidden = !running;
  $("stop").hidden = !running;

  const error = $("error");
  error.hidden = !status.message;
  error.textContent = status.message || "";

  const server = status.server;
  $("fact-name").textContent = server?.name ?? "-";
  $("fact-version").textContent = server?.version ?? "-";
  $("fact-uptime").textContent = server ? duration(server.uptime_seconds) : "-";
  $("fact-storage").textContent = server ? bytes(server.storage_bytes) : "-";
  $("fact-counts").textContent = server ? `${server.counts.libraries}개 · ${server.counts.documents}개` : "-";
  $("fact-online").textContent = server ? `${server.connected_devices}개` : "-";

  const list = $("addresses");
  list.replaceChildren(...status.addresses.map((address) => {
    const item = document.createElement("li");
    const code = document.createElement("code");
    code.textContent = address;
    const button = document.createElement("button");
    button.type = "button";
    button.className = "secondary";
    button.textContent = "복사";
    button.addEventListener("click", () => copy(address, "접속 주소를 복사했습니다."));
    item.append(code, button);
    return item;
  }));

  // 주소 목록이 바뀐 경우에만 다시 그려 선택 중인 목록이 닫히지 않게 한다.
  const select = $("pairing-url");
  const current = [...select.options].map((option) => option.value).join("\n");
  if (current !== status.addresses.join("\n")) {
    const selected = select.value;
    select.replaceChildren(...status.addresses.map((address) => new Option(address, address)));
    if (status.addresses.includes(selected)) select.value = selected;
  }

  const configured = server && !server.setup_required;
  $("setup").hidden = !(server && server.setup_required);
  $("password-form").hidden = !configured;
  for (const element of $("pairing-form").elements) element.disabled = !running;

  if (!settingsLoaded) {
    $("settings-port").value = status.settings.port;
    $("settings-lan").checked = status.settings.allow_lan;
    $("settings-autostart").checked = status.autostart;
    settingsLoaded = true;
  }
  $("data-dir").textContent = `데이터 폴더: ${status.data_dir}`;
}

function renderDevices(devices) {
  $("devices-empty").hidden = devices.length > 0;
  $("devices").replaceChildren(...devices.map((device) => {
    const item = document.createElement("li");
    const info = document.createElement("div");
    info.className = "device-info";
    const actions = document.createElement("div");
    actions.className = "row";

    if (editingDevice === device.id) {
      const input = document.createElement("input");
      input.value = device.name;
      input.maxLength = 100;
      info.append(input);
      const save = document.createElement("button");
      save.type = "button";
      save.textContent = "저장";
      save.addEventListener("click", () => run(save, async () => {
        await invoke("device_rename", { id: device.id, name: input.value });
        editingDevice = null;
        toast("기기 이름을 바꿨습니다.");
      }));
      const cancel = document.createElement("button");
      cancel.type = "button";
      cancel.className = "secondary";
      cancel.textContent = "취소";
      cancel.addEventListener("click", () => { editingDevice = null; refresh(); });
      actions.append(save, cancel);
      setTimeout(() => input.focus(), 0);
    } else {
      const name = document.createElement("span");
      name.className = "device-name";
      const dot = document.createElement("span");
      dot.className = device.connected ? "dot on" : "dot";
      name.append(dot, document.createTextNode(device.name));
      const meta = document.createElement("span");
      meta.className = "device-meta";
      const role = device.role === "admin" ? "관리자 기기" : "일반 기기";
      const platform = device.platform ? ` · ${device.platform}` : "";
      meta.textContent = `${role}${platform} · ${device.connected ? "실시간 연결 중" : seen(device.last_seen_at)}`;
      info.append(name, meta);

      const rename = document.createElement("button");
      rename.type = "button";
      rename.className = "secondary";
      rename.textContent = "이름 변경";
      rename.addEventListener("click", () => { editingDevice = device.id; refresh(); });
      const revoke = document.createElement("button");
      revoke.type = "button";
      revoke.className = "danger";
      revoke.textContent = "연결 해제";
      revoke.addEventListener("click", () => {
        if (!confirm(`'${device.name}' 기기의 연결을 해제할까요? 이 기기는 연결 코드로 다시 연결해야 합니다.`)) return;
        run(revoke, async () => {
          await invoke("device_revoke", { id: device.id });
          toast("연결을 해제했습니다.");
        });
      });
      actions.append(rename, revoke);
    }
    item.append(info, actions);
    return item;
  }));
}

function renderPairing() {
  clearInterval(pairingTimer);
  $("pairing-result").hidden = !pairing;
  if (!pairing) return;
  $("pairing-code").textContent = pairing.code;
  $("pairing-connection").value = pairing.connection;
  $("pairing-qr").innerHTML = pairing.qr_svg;
  const tick = () => {
    const left = Math.round(pairing.expires_at - Date.now() / 1000);
    if (left <= 0) {
      $("pairing-expiry").textContent = "유효 시간이 지났습니다. 새 코드를 발급해 주세요.";
      clearInterval(pairingTimer);
      return;
    }
    const minutes = Math.floor(left / 60);
    const seconds = String(left % 60).padStart(2, "0");
    $("pairing-expiry").textContent = `${minutes}:${seconds} 후 만료 · ${pairing.role === "admin" ? "관리자 기기" : "일반 기기"}`;
  };
  tick();
  pairingTimer = setInterval(tick, 1000);
}

async function refresh() {
  try {
    latest = await invoke("server_status");
    renderStatus(latest);
    if (latest.state === "running") {
      if (editingDevice === null) {
        const result = await invoke("devices_list");
        renderDevices(result.devices);
      }
    } else {
      renderDevices([]);
    }
  } catch (error) {
    failure(error);
  }
}

$("start").addEventListener("click", (event) => run(event.currentTarget, () => invoke("server_start")));
$("restart").addEventListener("click", (event) => run(event.currentTarget, () => invoke("server_restart")));
$("stop").addEventListener("click", (event) => run(event.currentTarget, () => invoke("server_stop")));
$("open-data").addEventListener("click", () => invoke("open_data_dir").catch(failure));
$("open-log").addEventListener("click", () => invoke("open_log").catch(failure));

$("setup-form").addEventListener("submit", (event) => {
  event.preventDefault();
  const password = $("setup-password").value;
  if (password !== $("setup-confirm").value) {
    toast("비밀번호 확인이 일치하지 않습니다.");
    return;
  }
  run(event.submitter, async () => {
    await invoke("admin_setup", { password });
    event.target.reset();
    toast("관리자 비밀번호를 설정했습니다.");
  });
});

$("password-form").addEventListener("submit", (event) => {
  event.preventDefault();
  run(event.submitter, async () => {
    await invoke("admin_password", { password: $("password-new").value });
    event.target.reset();
    toast("관리자 비밀번호를 바꿨습니다.");
  });
});

$("settings-form").addEventListener("submit", (event) => {
  event.preventDefault();
  run(event.submitter, async () => {
    await invoke("settings_save", {
      port: Number($("settings-port").value),
      allowLan: $("settings-lan").checked,
      autostart: $("settings-autostart").checked,
    });
    settingsLoaded = false;
    toast("설정을 저장했습니다.");
  });
});

$("pairing-form").addEventListener("submit", (event) => {
  event.preventDefault();
  run(event.submitter, async () => {
    pairing = await invoke("pairing_create", {
      name: $("pairing-name").value,
      role: $("pairing-role").value,
      serverUrl: $("pairing-url").value,
    });
    renderPairing();
  });
});

$("pairing-copy").addEventListener("click", () => {
  if (pairing) copy(pairing.connection, "연결 정보를 복사했습니다.");
});

$("pairing-cancel").addEventListener("click", (event) => {
  if (!pairing) return;
  run(event.currentTarget, async () => {
    try {
      await invoke("pairing_cancel", { id: pairing.id });
    } finally {
      pairing = null;
      renderPairing();
    }
  });
});

refresh();
setInterval(refresh, 3000);
