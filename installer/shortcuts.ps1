param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [ValidateSet('Remove','InspectDesktop')][string]$Mode='Remove'
)
$ErrorActionPreference='Stop'
$target=[IO.Path]::GetFullPath($Executable)
if([IO.Path]::GetFileName($target) -ne 'octopus-island.exe'){throw 'Shortcut target must be the application executable.'}
$shell=New-Object -ComObject WScript.Shell
$directories=@(
    [IO.Path]::GetFullPath($shell.SpecialFolders.Item('Desktop')),
    [IO.Path]::GetFullPath((Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'))
)
$desktopFound=$false
foreach($directory in $directories){
    if(-not(Test-Path -LiteralPath $directory -PathType Container)){continue}
    foreach($entry in Get-ChildItem -LiteralPath $directory -Filter '*.lnk' -File){
        # Enumerate only these known folders and delete only links targeting this installation.
        $path=[IO.Path]::GetFullPath($entry.FullName)
        if([IO.Path]::GetDirectoryName($path) -ne $directory){continue}
        try{
            $link=$shell.CreateShortcut($path)
            $destination=$link.TargetPath
            if([string]::IsNullOrWhiteSpace($destination)){
                # Some Windows locales cannot read Unicode shortcut filenames through WScript.
                $probe=Join-Path ([IO.Path]::GetTempPath()) ('octopus-shortcut-'+[Guid]::NewGuid().ToString('N')+'.lnk')
                try{
                    Copy-Item -LiteralPath $path -Destination $probe
                    $destination=$shell.CreateShortcut($probe).TargetPath
                }finally{if(Test-Path -LiteralPath $probe){Remove-Item -LiteralPath $probe}}
            }
            if([string]::IsNullOrWhiteSpace($destination)){continue}
            $linkTarget=[IO.Path]::GetFullPath($destination)
        }catch{continue}
        if($linkTarget -ne $target){continue}
        if($directory -eq $directories[0]){$desktopFound=$true}
        if($Mode -eq 'Remove'){Remove-Item -LiteralPath $path}
    }
}
if($Mode -eq 'InspectDesktop'){
    # Exit codes avoid console encoding differences when called by the native installer.
    if($desktopFound){exit 10}
    exit 0
}
