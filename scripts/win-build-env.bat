@echo off
REM win-build-env.bat — loads the MSVC arm64 + LLVM build env, then runs the remote build
REM under git-bash. Shipped FRESH from the Mac on every build (the target owns nothing).
REM The tauri/NSIS step needs link.exe (MSVC arm64) + clang (LLVM, for ring); vcvarsall
REM exports them into this cmd session and git-bash inherits the environment.
REM Usage: win-build-env.bat <server-nexe-commit> <nexe-app-commit>
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvarsall.bat" arm64
if errorlevel 1 (echo FATAL: vcvarsall arm64 failed & exit /b 1)
set "PATH=C:\Program Files\LLVM\bin;%PATH%"
"C:\Program Files\Git\bin\bash.exe" -lc "bash /c/nexe/_incoming/win-remote-build.sh %1 %2 %3"
