const $ = id => document.getElementById(id);
let setup = false;
async function post(path, body) {
  const response = await fetch(path, { method: "POST", credentials: "same-origin", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw data.error || `요청 실패 (${response.status})`;
  return data;
}
async function init() {
  try {
    const state = await (await fetch("/api/session", { credentials: "same-origin" })).json();
    if (state.authenticated) { location.href = "/"; return; }
    setup = state.setup;
    $("intro").textContent = setup ? "처음 한 번 관리자 계정을 만듭니다. 서버 로그의 ‘최초 설정 코드’를 입력하세요." : "관리자 계정으로 로그인하세요.";
    $("code-field").hidden = $("confirm-field").hidden = !setup;
    $("confirm").required = $("code").required = setup;
    $("password").autocomplete = setup ? "new-password" : "current-password";
    $("submit").textContent = setup ? "계정 만들기" : "로그인";
    $("form").hidden = false;
  } catch { $("intro").textContent = "서버에 연결하지 못했습니다."; }
}
$("form").addEventListener("submit", async event => {
  event.preventDefault(); $("message").textContent = "";
  if (setup && $("password").value !== $("confirm").value) { $("message").textContent = "비밀번호 확인이 일치하지 않습니다."; return; }
  $("submit").disabled = true;
  try {
    await post(setup ? "/api/setup" : "/api/login", setup
      ? { code: $("code").value, username: $("username").value, password: $("password").value }
      : { username: $("username").value, password: $("password").value });
    location.href = "/";
  } catch (error) { $("message").textContent = String(error); $("submit").disabled = false; }
});
init();
