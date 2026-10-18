fn main() {
    // 아이콘만 바뀌어도 실행 파일과 창·트레이 아이콘을 다시 만든다.
    println!("cargo:rerun-if-changed=icons");
    tauri_build::build()
}
