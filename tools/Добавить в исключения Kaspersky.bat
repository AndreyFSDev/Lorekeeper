@echo off
echo ========================================
echo   Добавление Lorekeeper в исключения Kaspersky
echo ========================================
echo.

REM Добавляем папку Node в исключения
reg add "HKLM\SOFTWARE\WOW6432Node\KasperskyLab\protected\KIS\21.26\settings\Exclusions\Paths\{E8A3B4F1-5C2D-4A6B-9F0E-1D3C5B7A9E8F}" /v "Path" /t REG_SZ /d "Z:\AImodels\Lorekeeper\runtime\node" /f
reg add "HKLM\SOFTWARE\WOW6432Node\KasperskyLab\protected\KIS\21.26\settings\Exclusions\Paths\{E8A3B4F1-5C2D-4A6B-9F0E-1D3C5B7A9E8F}" /v "Status" /t REG_DWORD /d 1 /f
reg add "HKLM\SOFTWARE\WOW6432Node\KasperskyLab\protected\KIS\21.26\settings\Exclusions\Paths\{E8A3B4F1-5C2D-4A6B-9F0E-1D3C5B7A9E8F}" /v "Mask" /t REG_SZ /d "*" /f

echo.
echo Добавлено: Z:\AImodels\Lorekeeper\runtime\node
echo.
echo Готово! Перезапустите Lorekeeper.
echo.
pause
