#Requires -Modules @{ ModuleName='Pester'; ModuleVersion='5.0.0' }
# Packaged provider registration and native source discovery only; no model requests.

BeforeDiscovery {
    Import-Module (Join-Path $PSScriptRoot '..\ItE2E\ItE2E.psd1') -Force
    $script:Ready = [bool](Resolve-ItApp -Package (Get-ItTestPackage) -IfInstalled)
}

Describe 'Feature Antigravity provider discovery' -Tag 'Feature', 'Antigravity' -Skip:(-not $script:Ready) {
    BeforeAll {
        Import-Module (Join-Path $PSScriptRoot '..\ItE2E\ItE2E.psd1') -Force
        $script:appInfo = Resolve-ItApp -Package (Get-ItTestPackage)
        # Managed-cache discovery must retain the selected package's identity.
        [xml]$manifest = Get-Content -LiteralPath (Join-Path $script:appInfo.InstallLocation 'AppxManifest.xml') -Raw
        $aliases = @($manifest.SelectNodes(
            "//*[local-name()='Extension' and @Category='windows.appExecutionAlias' and @Executable='wta.exe']/*[local-name()='AppExecutionAlias']/*[local-name()='ExecutionAlias']"
        ))
        $aliases.Count | Should -Be 1 -Because 'the selected package must declare one WTA CLI alias'
        $script:wta = Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps\$($script:appInfo.Package)\$($aliases[0].GetAttribute('Alias'))"
        Test-Path -LiteralPath $script:wta -PathType Leaf |
            Should -BeTrue -Because 'the availability probe must use this package family, not an unpackaged binary'
        if ($env:ITE2E_EXPECTED_WTA_SHA256) {
            (Get-FileHash -LiteralPath $script:appInfo.WtaPath -Algorithm SHA256).Hash |
                Should -Be $env:ITE2E_EXPECTED_WTA_SHA256 -Because 'the packaged probe must match the intended source build'
        }
    }

    It 'Antigravity is registered as a standalone ACP provider' {
        $probe = Invoke-Native -FilePath $script:wta -Arguments @('probe-host-agents') -TimeoutSec 30
        $probe.ExitCode | Should -Be 0 -Because 'the packaged availability probe must complete'
        $snapshot = $probe.StdOut | ConvertFrom-Json -Depth 20
        $entry = @($snapshot.availability | Where-Object id -CEQ 'antigravity')
        $entry.Count | Should -Be 1 -Because 'the built-in family must cross the packaged WTA availability boundary'
        $entry[0].display_name | Should -BeExactly 'Google Antigravity'
        $entry[0].requires_npx | Should -BeFalse
        $entry[0].launch_ready | Should -Be $entry[0].native_cli_found
        @($snapshot.availability.id) | Should -Contain 'copilot'
    }

    It 'Antigravity WSL discovery requires the native Linux ACP executable' {
        $distro = $env:ITE2E_ANTIGRAVITY_WSL_DISTRO
        if ([string]::IsNullOrWhiteSpace($distro)) {
            Set-ItResult -Skipped -Because 'set ITE2E_ANTIGRAVITY_WSL_DISTRO to an installed native ACP test environment'
            return
        }
        $configuredServer = $env:ITE2E_ANTIGRAVITY_WSL_ACP_PATH
        if ([string]::IsNullOrWhiteSpace($configuredServer)) {
            $native = Invoke-Native -FilePath 'wsl.exe' -Arguments @(
                '-d', $distro, '--exec', 'bash', '-lc', 'command -v agy_acp_server.par'
            ) -TimeoutSec 30
            if ($native.ExitCode -ne 0 -or -not $native.StdOut.Trim()) {
                Set-ItResult -Skipped -Because 'the selected distro needs a native ACP server on PATH or ITE2E_ANTIGRAVITY_WSL_ACP_PATH'
                return
            }
        } else {
            $native = Invoke-Native -FilePath 'wsl.exe' -Arguments @(
                '-d', $distro, '--exec', 'readlink', '-f', '--', $configuredServer
            ) -TimeoutSec 30
            $native.ExitCode | Should -Be 0 -Because 'the explicitly selected managed installation must resolve'
        }
        $server = $native.StdOut.Trim()
        $server | Should -Match '^/' -Because 'the native server must resolve to an absolute Linux path'
        $companion = $server.Substring(0, $server.LastIndexOf('/')) + '/localharness_external'
        foreach ($binary in @($server, $companion)) {
            $resolved = Invoke-Native -FilePath 'wsl.exe' -Arguments @(
                '-d', $distro, '--exec', 'readlink', '-f', '--', $binary
            ) -TimeoutSec 30
            $resolved.ExitCode | Should -Be 0
            $path = $resolved.StdOut.Trim()
            $path | Should -Match '^/'
            $path | Should -Not -Match '^/mnt/' -Because 'Windows interop cannot substitute for either native ACP executable'
            foreach ($check in @('-f', '-x')) {
                $file = Invoke-Native -FilePath 'wsl.exe' -Arguments @(
                    '-d', $distro, '--exec', 'test', $check, $path
                ) -TimeoutSec 30
                $file.ExitCode | Should -Be 0 -Because 'both the server and companion must be executable Linux files'
            }
        }
        $probe = Invoke-Native -FilePath $script:wta -Arguments @(
            'probe-agent-sources', '--wsl-distro', $distro
        ) -TimeoutSec 45
        $probe.ExitCode | Should -Be 0
        $snapshot = $probe.StdOut | ConvertFrom-Json -Depth 20
        $snapshot.wsl_distro | Should -BeExactly $distro
        @($snapshot.agents | ForEach-Object { $_.id }) |
            Should -Contain 'antigravity' -Because 'native ACP discovery must not look for a program named after the provider ID'
    }
}
