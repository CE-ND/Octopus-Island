# Codex 官方 hooks 的本地状态桥接：只保留状态、工具名、会话标识和项目文件夹名。
# 不保存提示词、工具参数或回复内容；不访问网络、不输出授权决定。
$ErrorActionPreference = 'Stop'
try {
    $eventTime = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $reader = New-Object IO.StreamReader([Console]::OpenStandardInput(), (New-Object Text.UTF8Encoding($false)))
    $data = $reader.ReadToEnd() | ConvertFrom-Json
    $session = [string]$data.session_id
    if ($session -notmatch '^[A-Za-z0-9_-]{1,80}$') { exit 0 }
    # 子助手的 hook 使用父会话 ID；只提示主会话，避免子任务覆盖父任务状态。
    if ($data.agent_id) { exit 0 }
    $tool = ''
    $kind = ''
    switch ([string]$data.hook_event_name) {
        'UserPromptSubmit' { $kind = 'working' }
        'PermissionRequest' { $kind = 'permission'; $tool = [string]$data.tool_name }
        'PreToolUse' {
            $tool = [string]$data.tool_name
            if ($tool -match '(^|[.:])request_user_input_async$') { $kind = 'question_async' }
            elseif ($tool -match '(^|[.:])request_user_input$') { $kind = 'question' }
            else { $kind = 'tool_start' }
        }
        'PostToolUse' { $kind = 'tool_end' }
        'Stop' { $kind = 'done' }
        'Interrupt' { $kind = 'interrupted' }
        'SessionEnd' { $kind = 'end' }
    }
    if (!$kind) { exit 0 }
    Publish-IslandAgentEvent -Data $data -Agent 'codex' -Kind $kind -DirectoryName 'octopus-island-codex-events' -EventTime $eventTime
} catch {
    # 状态提醒失败不影响 Codex 工作，也不把错误细节输出给模型。
}
exit 0
