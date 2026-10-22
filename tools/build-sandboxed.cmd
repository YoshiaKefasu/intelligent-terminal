@echo off
rem ============================================================================
rem build-sandboxed.cmd
rem
rem Runs MSBuild / build scripts pinned to a subset of CPU cores (Affinity Mask)
rem and below-normal priority so user foreground tasks (Premiere Pro, browser, etc.)
rem never lag or stutter.
rem
rem On this machine (8 logical processors), Affinity 0x0F restricts builds to
rem cores 0-3, leaving cores 4-7 completely free for user applications.
rem ============================================================================

cd /d "%~dp0.."

set AFFINITY_MASK=0x0F
set TARGET_SCRIPT=%~1
if "%TARGET_SCRIPT%"=="" set TARGET_SCRIPT=_build_msix_x64.cmd

echo [SANDBOX] Launching %TARGET_SCRIPT% on CPU cores 0-3 (Affinity: %AFFINITY_MASK%, BelowNormal Priority)...

start /b /wait /below /affinity %AFFINITY_MASK% cmd /c "%TARGET_SCRIPT%" %2 %3 %4 %5
set BUILD_EXIT=%ERRORLEVEL%

echo [SANDBOX] Build finished with exit code: %BUILD_EXIT%
exit /b %BUILD_EXIT%
