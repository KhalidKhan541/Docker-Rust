# Source this wherever cargo is invoked from a fresh shell:
#   . E:\Docker-Rust\tools\env.ps1
# Adds the rustup cargo bin, the toolchain self-contained dir (libgcc,
# libwinpthread, and rustup's bundled binutils), and MSYS2 mingw-w64
# binutils (provides as.exe which rustup's bundled dlltool shells out to).
$cargoBin = "C:\Users\khali\.cargo\bin"
$selfDir = "C:\Users\khali\.rustup\toolchains\stable-x86_64-pc-windows-gnu\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"
$msys2Bin = "E:\Docker-Rust\tools\mingw64\mingw64\bin"
$env:PATH = "$cargoBin;$selfDir;$msys2Bin;$env:PATH"
