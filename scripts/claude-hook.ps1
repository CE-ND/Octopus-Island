# 把 Claude Code hooks 给出的状态交给岛仔：只保存事件类型、工具名和项目文件夹名。
# 不保存提示词、命令、回复内容，不读取登录令牌，也不发送网络请求；不向标准输出写任何内容。
$ErrorActionPreference = 'Stop'
try {
    $eventTime = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    # Windows PowerShell 默认按系统代码页读标准输入，中文路径会乱码，这里固定按 UTF-8 读。
    $reader = New-Object IO.StreamReader([Console]::OpenStandardInput(), (New-Object Text.UTF8Encoding($false)))
    $data = $reader.ReadToEnd() | ConvertFrom-Json
    $session = [string]$data.session_id
    if ($session -notmatch '^[A-Za-z0-9_-]{1,80}$') { exit 0 }
    if ($data.agent_id) { exit 0 }
    $tool = ''
    $kind = ''
    switch ([string]$data.hook_event_name) {
        'UserPromptSubmit' { $kind = 'working' }
        'Stop' { $kind = 'done' }
        'SessionEnd' { $kind = 'end' }
        'PermissionRequest' { $kind = 'permission'; $tool = [string]$data.tool_name }
        'PreToolUse' {
            $tool = [string]$data.tool_name
            if ($tool -eq 'AskUserQuestion') { $kind = 'question' }
            elseif ($tool -eq 'ExitPlanMode') { $kind = 'plan' }
            else { $kind = 'tool_start' }
        }
        'PostToolUse' { $kind = 'tool_end' }
        'PostToolUseFailure' { $kind = 'tool_end' }
        'StopFailure' { $kind = 'failed' }
        'Notification' {
            switch ([string]$data.notification_type) {
                'permission_prompt' { $kind = 'permission' }
                'idle_prompt' { $kind = 'idle' }
                'elicitation_dialog' { $kind = 'question' }
            }
        }
    }
    if (!$kind) { exit 0 }
    Publish-IslandAgentEvent -Data $data -Agent 'claude-code' -Kind $kind -DirectoryName 'octopus-island-events' -EventTime $eventTime
} catch {
    # 任何失败都保持安静，绝不影响 Claude Code 本身。
}
exit 0
