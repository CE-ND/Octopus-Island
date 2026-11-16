# 安装时前置到各助手脚本。只传递运行元数据，独立事件文件避免并行调用相互覆盖。
function Publish-IslandAgentEvent {
    param($Data, [string]$Agent, [string]$Kind, [string]$DirectoryName, [long]$EventTime)
    $session = [string]$Data.session_id
    if ($session -notmatch '^[A-Za-z0-9_-]{1,80}$') { return }
    $dir = Join-Path $PSScriptRoot $DirectoryName
    if (!(Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    $tool = [string]$Data.tool_name
    if ($tool -notmatch '^[A-Za-z0-9_.:-]{1,80}$') { $tool = '' }
    $project = ''
    if ($Data.cwd) { $project = [string](Split-Path ([string]$Data.cwd) -Leaf) }
    $project = $project -replace '[\x00-\x1f\x7f]', ''
    if ($project.Length -gt 60) { $project = $project.Substring(0, 60) }
    $turn = [string]$Data.turn_id
    if ($turn -notmatch '^[A-Za-z0-9_.:-]{1,100}$') { $turn = '' }
    $call = [string]$Data.tool_use_id
    if (!$call) { $call = [string]$Data.tool_call_id }
    if ($call -notmatch '^[A-Za-z0-9_.:-]{1,120}$') { $call = '' }

    # 每轮开始时识别实际助手进程，后续工具事件复用。只读取 PID、名称和创建时间。
    $ownerPath = Join-Path $dir ($session + '.owner')
    $owner = $null
    if ($Kind -eq 'working') {
        try {
            $ancestorId = $PID
            for ($depth = 0; $depth -lt 6; $depth++) {
                $ancestor = Get-CimInstance -ClassName Win32_Process -Filter ('ProcessId=' + $ancestorId) -OperationTimeoutSec 1 -ErrorAction Stop
                if (!$ancestor) { break }
                if ($depth -gt 0 -and $ancestor.Name -match '^(codex|claude|node)(\.exe)?$') {
                    $owner = @{ pid = [uint32]$ancestor.ProcessId; started = ([DateTimeOffset]$ancestor.CreationDate).ToUnixTimeMilliseconds() }
                    break
                }
                $ancestorId = [uint32]$ancestor.ParentProcessId
                if (!$ancestorId) { break }
            }
        } catch { }
        try {
            [IO.File]::WriteAllText($ownerPath, (ConvertTo-Json -InputObject $owner -Compress), (New-Object Text.UTF8Encoding($false)))
        } catch { }
    } elseif (Test-Path -LiteralPath $ownerPath) {
        try { $owner = [IO.File]::ReadAllText($ownerPath) | ConvertFrom-Json } catch { }
    }
    $record = [ordered]@{
        version = 2; agent = $Agent; kind = $Kind; tool = $tool
        session = $session; project = $project; turn = $turn; call = $call; at = $EventTime
    }
    if ($owner -and $owner.pid -and $owner.started) {
        $record.owner_pid = $owner.pid
        $record.owner_started = $owner.started
    }
    $path = Join-Path $dir ($session + '.' + $EventTime + '.' + [Guid]::NewGuid().ToString('N') + '.event')
    $temporary = $path + '.tmp'
    try {
        [IO.File]::WriteAllText($temporary, ($record | ConvertTo-Json -Compress), (New-Object Text.UTF8Encoding($false)))
        Move-Item -LiteralPath $temporary -Destination $path -Force
    } finally {
        if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary }
    }
    if ($Kind -eq 'end' -and (Test-Path -LiteralPath $ownerPath)) { Remove-Item -LiteralPath $ownerPath }
    # 岛仔未运行时也限制积压数量；运行时事件由监听器读后删除，空闲不启动定时扫描。
    $pending = @([IO.Directory]::EnumerateFiles($dir, '*.event') | Select-Object -First 513)
    if ($pending.Count -gt 512) {
        Get-ChildItem -LiteralPath $dir -Filter '*.event' -File | Sort-Object LastWriteTimeUtc -Descending |
            Select-Object -Skip 512 | ForEach-Object { Remove-Item -LiteralPath $_.FullName -ErrorAction SilentlyContinue }
    }
}
