# ช่วยพัฒนาแอปจดประชุม

1. ดูเฟสปัจจุบันจาก [README](README.md) หรือเปิด [บอร์ดติดตามงาน](project-tracker.html) ในเบราว์เซอร์
2. เปิด Issue หรือแจ้งใน Issue เดิมก่อนเริ่มงาน แล้ว fork repo และสร้าง branch สำหรับการเปลี่ยนแปลงนั้น
3. ส่ง Pull Request พร้อมเลข Issue สรุปสิ่งที่เปลี่ยน วิธีทดสอบ และผลทดสอบบน Windows หากเกี่ยวข้อง

หน้าคอนโซลอยู่ใน `ui/` และฝังในไฟล์โปรแกรม Rust ใช้ `cargo run --locked --bin meeting-notes` เพื่อเปิดแอป หรือเพิ่ม `-- --no-open` เพื่อให้พิมพ์ URL ในเครื่องโดยไม่เปิด Edge อัตโนมัติ ตั้ง `MEETING_NOTES_DATA_DIR` ได้สำหรับแยกข้อมูลทดสอบจากการประชุมจริง

## ตรวจโค้ด Rust บน Windows

ติดตั้ง Rust ผ่าน [Rustup](https://rustup.rs/) และติดตั้ง Visual Studio Build Tools ที่มี C++ และ Windows SDK เมื่อใช้ Rust แบบ MSVC จากนั้นรันที่โฟลเดอร์โปรเจกต์ ตัว Rustup จะเลือกเวอร์ชันตาม `rust-toolchain.toml`

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked
```

ใช้ไฟล์เสียงหรือข้อความที่สร้างขึ้นเพื่อทดสอบเมื่อแนบตัวอย่างสาธารณะ
