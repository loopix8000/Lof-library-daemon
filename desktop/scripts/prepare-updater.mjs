import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const desktop = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const root = path.resolve(desktop, "..");
const config = JSON.parse(fs.readFileSync(path.join(desktop, "src-tauri/tauri.conf.json"), "utf8"));
const pkg = JSON.parse(fs.readFileSync(path.join(desktop, "package.json"), "utf8"));
const cargo = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");
const version = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
const publicKey = process.env.TAURI_UPDATER_PUBLIC_KEY?.trim();

if (!publicKey || publicKey !== config.plugins.updater.pubkey) {
  throw new Error("TAURI_UPDATER_PUBLIC_KEY 변수가 앱에 설정한 공개키와 일치해야 합니다.");
}
if (pkg.version !== config.version || config.version !== version) {
  throw new Error("Cargo.toml, desktop/package.json, tauri.conf.json의 버전을 일치시키세요.");
}

// 일반 폴더 배포본과 분리하여 서명된 릴리즈 빌드에만 적용한다.
fs.writeFileSync(path.join(desktop, "src-tauri/tauri.updater.generated.json"), JSON.stringify({
  bundle: { createUpdaterArtifacts: true },
}, null, 2) + "\n");
if (process.env.GITHUB_OUTPUT) {
  fs.appendFileSync(process.env.GITHUB_OUTPUT, `version=${version}\nprerelease=${version.includes("-")}\n`);
}
console.log("서명된 설치 파일과 업데이트 정보를 만들 준비를 마쳤습니다.");
