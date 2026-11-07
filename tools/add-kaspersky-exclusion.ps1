# Adds the Cloufam folder to Kaspersky exclusions
# Run as administrator: right-click → Run as administrator

$exclusionPath = "Z:\AImodels\Cloufam\runtime\node"
$nodeExe = "Z:\AImodels\Cloufam\runtime\node\node.exe"

Write-Host "=== Добавление исключений Kaspersky ===" -ForegroundColor Cyan
Write-Host ""

# Method 1: through the Kaspersky registry
$regPaths = @(
    "HKLM:\SOFTWARE\WOW6432Node\KasperskyLab\protected\KIS\21.26\settings\Exclusions\Paths",
    "HKLM:\SOFTWARE\WOW6432Node\KasperskyLab\protected\KAV\21.26\settings\Exclusions\Paths",
    "HKLM:\SOFTWARE\WOW6432Node\KasperskyLab\AVP21.26\settings\Exclusions\Paths"
)

$added = $false
foreach ($regPath in $regPaths) {
    if (-not (Test-Path $regPath)) {
        try {
            New-Item -Path $regPath -Force -ErrorAction Stop | Out-Null
            Write-Host "Создан ключ: $regPath" -ForegroundColor Green
        } catch {
            Write-Host "Не удалось создать: $regPath — $_" -ForegroundColor Yellow
            continue
        }
    }
    
    # Add the folder
    try {
        $guid = [guid]::NewGuid().ToString()
        $entryPath = "$regPath\$guid"
        New-Item -Path $entryPath -Force -ErrorAction Stop | Out-Null
        Set-ItemProperty -Path $entryPath -Name "Path" -Value $exclusionPath -ErrorAction Stop
        Set-ItemProperty -Path $entryPath -Name "Status" -Value 1 -ErrorAction Stop
        Set-ItemProperty -Path $entryPath -Name "Mask" -Value "*" -ErrorAction Stop
        Write-Host "Добавлено исключение: $exclusionPath" -ForegroundColor Green
        $added = $true
    } catch {
        Write-Host "Не удалось добавить в $regPath — $_" -ForegroundColor Yellow
    }
    
    # Add the exe
    try {
        $guid2 = [guid]::NewGuid().ToString()
        $entryPath2 = "$regPath\$guid2"
        New-Item -Path $entryPath2 -Force -ErrorAction Stop | Out-Null
        Set-ItemProperty -Path $entryPath2 -Name "Path" -Value $nodeExe -ErrorAction Stop
        Set-ItemProperty -Path $entryPath2 -Name "Status" -Value 1 -ErrorAction Stop
        Write-Host "Добавлено исключение: $nodeExe" -ForegroundColor Green
        $added = $true
    } catch {
        Write-Host "Не удалось добавить в $regPath — $_" -ForegroundColor Yellow
    }
}

# Method 2: through Windows Defender (in case Kaspersky does not work)
Write-Host ""
Write-Host "=== Добавление в Windows Defender ===" -ForegroundColor Cyan
try {
    Add-MpPreference -ExclusionPath $exclusionPath -ErrorAction Stop
    Write-Host "Добавлено в Windows Defender: $exclusionPath" -ForegroundColor Green
} catch {
    Write-Host "Windows Defender: $_" -ForegroundColor Yellow
}

if ($added) {
    Write-Host ""
    Write-Host "=== Готово! ===" -ForegroundColor Green
    Write-Host "Перезапустите Cloufam для применения изменений." -ForegroundColor Cyan
} else {
    Write-Host ""
    Write-Host "=== Не удалось автоматически добавить исключения ===" -ForegroundColor Red
    Write-Host "Добавьте вручную в Kaspersky:" -ForegroundColor Yellow
    Write-Host "  Настройки → Защита → Исключения → Добавить:" -ForegroundColor Yellow
    Write-Host "  Папка: $exclusionPath" -ForegroundColor White
    Write-Host "  Файл:  $nodeExe" -ForegroundColor White
}

Write-Host ""
Write-Host "Нажмите любую клавишу для выхода..."
$null = $Host.UI.RawUI.ReadKey("NoEcho,IncludeKeyDown")

