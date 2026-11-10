# Signs a DiskVibe build with the project's self-signed code-signing certificate.
#
#   powershell -ExecutionPolicy Bypass -File scripts\sign.ps1 [-File target\release\diskvibe.exe]
#
# Locally it reads signing\diskvibe-signing.pfx and signing\password.txt (both kept out of git).
# On the build machines it reads the WINDOWS_CERT_PFX (base64) and WINDOWS_CERT_PASSWORD secrets.
# With neither present it does nothing, so forks still build.
param([string]$File = "target\release\diskvibe.exe")
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$flags = [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]::EphemeralKeySet

if ($env:WINDOWS_CERT_PFX) {
    $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 -ArgumentList @([Convert]::FromBase64String($env:WINDOWS_CERT_PFX), $env:WINDOWS_CERT_PASSWORD)
} elseif (Test-Path "$root\signing\diskvibe-signing.pfx") {
    $pw = (Get-Content "$root\signing\password.txt" -Raw).Trim()
    $cert = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 -ArgumentList @("$root\signing\diskvibe-signing.pfx", $pw)
} else {
    Write-Host "No signing certificate available; leaving $File unsigned."
    exit 0
}

$sig = Set-AuthenticodeSignature -FilePath $File -Certificate $cert -HashAlgorithm SHA256 -TimestampServer "http://timestamp.digicert.com"
# A self-signed certificate is not chained to a trusted root, so Windows reports the signature as
# present but untrusted ("UnknownError") unless packaging\DiskVibe-signing.cer has been trusted.
if (-not $sig.SignerCertificate) { throw "Signing $File failed: $($sig.StatusMessage)" }
Write-Host "Signed $File  ($($sig.SignerCertificate.Subject), thumbprint $($sig.SignerCertificate.Thumbprint), status: $($sig.Status))"
