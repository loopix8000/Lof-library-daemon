import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktopDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const workspaceDir = path.resolve(desktopDir, "..");
const pkg = JSON.parse(fs.readFileSync(path.join(desktopDir, "package.json"), "utf8"));
const mode = process.argv[2] || "dir";

// 앱 버전이나 저장소 위치가 바뀌어도 기존 폴더 배포본의 위치를 유지한다.
const releaseDir = "C:\\Projects\\lof-library-daemon\\release";
const unpackedDir = path.join(releaseDir, "win-unpacked");
const releaseTarget = path.join(workspaceDir, "target", "release");
const builtExe = path.join(releaseTarget, process.platform === "win32" ? "lof-library-desktop.exe" : "lof-library-desktop");
const targetExeName = process.platform === "win32" ? "Lof Library.exe" : "Lof Library";

function assertChildPath(parent, target) {
  const base = fs.realpathSync(parent);
  const resolved = fs.realpathSync(target);
  const relative = path.relative(base, resolved);
  if (!relative || relative === ".." || relative.startsWith(`..${path.sep}`) || path.isAbsolute(relative)) {
    throw new Error(`배포 폴더 밖의 경로는 지우지 않습니다: ${resolved}`);
  }
}

if (!["dir", "nsis"].includes(mode)) {
  console.error(`지원하지 않는 배포 방식입니다: ${mode} (dir 또는 nsis)`);
  process.exit(1);
}

const tauriCli = path.join(desktopDir, "node_modules", "@tauri-apps", "cli", "tauri.js");
if (!fs.existsSync(tauriCli)) {
  console.error("Tauri CLI가 없습니다. desktop 폴더에서 npm install을 먼저 실행해 주세요.");
  process.exit(1);
}

// 지난 배포본과 아이콘 내용이 다르면 빌드가 끝난 뒤 Windows 아이콘 캐시도 비운다.
const iconPath = path.join(desktopDir, "src-tauri", "icons", "icon.ico");
const iconStamp = path.join(releaseDir, ".icon-sha256");
const iconHash = createHash("sha256").update(fs.readFileSync(iconPath)).digest("hex");
const iconChanged = !fs.existsSync(iconStamp) || fs.readFileSync(iconStamp, "utf8").trim() !== iconHash;

const tauriArgs = mode === "nsis" ? [tauriCli, "build", "--bundles", "nsis"] : [tauriCli, "build", "--no-bundle"];
const result = spawnSync(process.execPath, tauriArgs, { cwd: desktopDir, stdio: "inherit" });
if (result.status !== 0) {
  process.exit(result.status ?? 1);
}
if (!fs.existsSync(builtExe)) {
  console.error(`빌드한 실행 파일을 찾지 못했습니다: ${builtExe}`);
  process.exit(1);
}

// 실행 중인 이전 배포본이 있으면 파일을 교체할 수 없으므로 먼저 종료한다.
if (process.platform === "win32") {
  spawnSync("taskkill", ["/F", "/IM", targetExeName], { stdio: "ignore" });
}

fs.mkdirSync(unpackedDir, { recursive: true });
assertChildPath(releaseDir, unpackedDir);
for (const entry of fs.readdirSync(unpackedDir)) {
  assertChildPath(unpackedDir, path.join(unpackedDir, entry));
}
for (const entry of fs.readdirSync(unpackedDir)) {
  fs.rmSync(path.join(unpackedDir, entry), { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
fs.copyFileSync(builtExe, path.join(unpackedDir, targetExeName));

if (mode === "nsis") {
  const nsisDir = path.join(releaseTarget, "bundle", "nsis");
  for (const file of fs.existsSync(nsisDir) ? fs.readdirSync(nsisDir) : []) {
    if (file.endsWith(".exe")) {
      fs.copyFileSync(path.join(nsisDir, file), path.join(releaseDir, file));
      console.log(`설치 파일: ${path.join(releaseDir, file)}`);
    }
  }
}

if (process.platform === "win32" && iconChanged) {
  // 같은 경로의 실행 파일은 탐색기가 옛 아이콘을 계속 보여 주므로 고정된 바로 가기와 아이콘 캐시를 정리한다.
  const pinned = path.join(process.env.APPDATA || "", "Microsoft", "Internet Explorer", "Quick Launch", "User Pinned", "ImplicitAppShortcuts");
  for (const folder of fs.existsSync(pinned) ? fs.readdirSync(pinned) : []) {
    const shortcut = path.join(pinned, folder, `${path.parse(targetExeName).name}.lnk`);
    if (fs.existsSync(shortcut)) fs.rmSync(shortcut, { force: true });
  }
  const explorerCache = path.join(process.env.LOCALAPPDATA || "", "Microsoft", "Windows", "Explorer");
  for (const file of fs.existsSync(explorerCache) ? fs.readdirSync(explorerCache) : []) {
    if (!file.startsWith("iconcache_") || !file.endsWith(".db")) continue;
    try {
      fs.rmSync(path.join(explorerCache, file), { force: true });
    } catch {
      // 탐색기가 사용 중인 캐시는 건너뛴다.
    }
  }
  spawnSync("ie4uinit.exe", ["-ClearIconCache"], { stdio: "ignore", timeout: 3000 });
  spawnSync("ie4uinit.exe", ["-show"], { stdio: "ignore", timeout: 3000 });
  console.log("아이콘이 바뀌어 Windows 아이콘 캐시를 정리했습니다.");
}
fs.writeFileSync(iconStamp, iconHash);

console.log(`Lof Library ${pkg.version} 폴더 배포본: ${unpackedDir}`);
