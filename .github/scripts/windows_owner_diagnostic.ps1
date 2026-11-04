$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Principal;

public static class OwnerProbe {
    [StructLayout(LayoutKind.Sequential)]
    struct SecurityAttributes { public uint length; public IntPtr descriptor; public int inherit; }
    [DllImport("kernel32.dll")] static extern IntPtr GetCurrentProcess();
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr handle);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr memory);
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool CreateDirectoryW(string path, ref SecurityAttributes attributes);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool GetTokenInformation(IntPtr token, int kind, IntPtr data, int size, out int needed);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool DuplicateTokenEx(IntPtr token, uint access, IntPtr attributes, int level, int type, out IntPtr duplicate);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool SetTokenInformation(IntPtr token, int kind, IntPtr data, int length);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool SetThreadToken(IntPtr thread, IntPtr token);
    [DllImport("advapi32.dll", SetLastError=true)] static extern bool RevertToSelf();
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool ConvertStringSidToSidW(string text, out IntPtr sid);
    [DllImport("advapi32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string text, uint revision, out IntPtr descriptor, IntPtr size);

    static IntPtr Token(uint access) {
        IntPtr token;
        if (!OpenProcessToken(GetCurrentProcess(), access, out token)) throw new Win32Exception(Marshal.GetLastWin32Error());
        return token;
    }
    public static string Sid(int kind) {
        IntPtr token = Token(8);
        try {
            int needed;
            GetTokenInformation(token, kind, IntPtr.Zero, 0, out needed);
            if (needed <= 0) throw new Win32Exception(Marshal.GetLastWin32Error());
            IntPtr data = Marshal.AllocHGlobal(needed);
            try {
                if (!GetTokenInformation(token, kind, data, needed, out needed)) throw new Win32Exception(Marshal.GetLastWin32Error());
                return new SecurityIdentifier(Marshal.ReadIntPtr(data)).Value;
            } finally { Marshal.FreeHGlobal(data); }
        } finally { CloseHandle(token); }
    }
    public static int Elevation() {
        IntPtr token = Token(8);
        IntPtr data = Marshal.AllocHGlobal(4);
        try {
            int needed;
            if (!GetTokenInformation(token, 20, data, 4, out needed)) throw new Win32Exception(Marshal.GetLastWin32Error());
            return Marshal.ReadInt32(data);
        } finally { Marshal.FreeHGlobal(data); CloseHandle(token); }
    }
    public static void Create(string path, string sddl, bool forceAdministratorsOwner) {
        IntPtr token = IntPtr.Zero, duplicate = IntPtr.Zero, sid = IntPtr.Zero, descriptor = IntPtr.Zero, owner = IntPtr.Zero;
        bool impersonating = false;
        try {
            if (forceAdministratorsOwner) {
                token = Token(8 | 2);
                if (!DuplicateTokenEx(token, 8 | 0x80 | 4, IntPtr.Zero, 2, 2, out duplicate)) throw new Win32Exception(Marshal.GetLastWin32Error());
                if (!ConvertStringSidToSidW("S-1-5-32-544", out sid)) throw new Win32Exception(Marshal.GetLastWin32Error());
                owner = Marshal.AllocHGlobal(IntPtr.Size);
                Marshal.WriteIntPtr(owner, sid);
                if (!SetTokenInformation(duplicate, 4, owner, IntPtr.Size)) throw new Win32Exception(Marshal.GetLastWin32Error());
                if (!SetThreadToken(IntPtr.Zero, duplicate)) throw new Win32Exception(Marshal.GetLastWin32Error());
                impersonating = true;
            }
            if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl, 1, out descriptor, IntPtr.Zero)) throw new Win32Exception(Marshal.GetLastWin32Error());
            var attributes = new SecurityAttributes { length = (uint)Marshal.SizeOf<SecurityAttributes>(), descriptor = descriptor, inherit = 0 };
            if (!CreateDirectoryW(path, ref attributes)) throw new Win32Exception(Marshal.GetLastWin32Error());
        } finally {
            if (impersonating && !RevertToSelf()) throw new Win32Exception(Marshal.GetLastWin32Error());
            if (descriptor != IntPtr.Zero) LocalFree(descriptor);
            if (owner != IntPtr.Zero) Marshal.FreeHGlobal(owner);
            if (sid != IntPtr.Zero) LocalFree(sid);
            if (duplicate != IntPtr.Zero) CloseHandle(duplicate);
            if (token != IntPtr.Zero) CloseHandle(token);
        }
    }
}
'@
$userSid = [OwnerProbe]::Sid(1)
$ownerSid = [OwnerProbe]::Sid(4)
$groupSid = [OwnerProbe]::Sid(5)
Write-Host "TOKEN user=$userSid default_owner=$ownerSid primary_group=$groupSid elevated=$([OwnerProbe]::Elevation())"
$dacl = 'D:P(A;;GA;;;OW)(A;;GA;;;SY)'
$paths = @()
try {
    foreach ($forced in @($false, $true)) {
        foreach ($explicit in @($false, $true)) {
            $path = Join-Path $env:TEMP ('lxs-owner-diag-' + [guid]::NewGuid().ToString('N'))
            $sddl = if ($explicit) { 'O:' + $userSid + $dacl } else { $dacl }
            try {
                [OwnerProbe]::Create($path, $sddl, $forced)
                $paths += $path
                $acl = Get-Acl -LiteralPath $path
                $actualOwner = $acl.GetOwner([System.Security.Principal.SecurityIdentifier]).Value
                $actualDacl = $acl.GetSecurityDescriptorSddlForm([System.Security.AccessControl.AccessControlSections]::Access)
                Write-Host "OWNER_DIAG force_admin=$forced explicit_user=$explicit owner=$actualOwner owner_matches_user=$($actualOwner -eq $userSid) dacl=$actualDacl"
                if ($explicit -and $actualOwner -ne $userSid) { throw 'explicit user owner did not take effect' }
                if ($forced -and -not $explicit -and $actualOwner -ne 'S-1-5-32-544') { throw 'forced default owner did not take effect' }
                if ($actualDacl.Replace(';;FA;;;',';;GA;;;') -ne $dacl) { throw 'private DACL changed' }
            } catch {
                if ($forced -and -not (Test-Path -LiteralPath $path)) {
                    Write-Host "OWNER_DIAG force_admin=$forced explicit_user=$explicit unavailable=$($_.Exception.Message)"
                } else { throw }
            }
        }
    }
} finally {
    foreach ($path in $paths) { Remove-Item -LiteralPath $path -Force }
}
