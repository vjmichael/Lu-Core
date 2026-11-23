# Owner test: do Word, PowerPoint and Excel open the files Lu creates?
#
# Run on a PC with Microsoft Office, from any folder:
#   powershell -ExecutionPolicy Bypass -File C:\Lu\scripts\owner-tests\office-files.ps1
#
# What it does:
#   1. Builds the three sample files with Lu's own code (cargo test, the
#      ignored write_samples test) into a new temporary folder.
#   2. Opens each one read-only in its Office app, invisibly, with macros
#      off, counts paragraphs / slides / sheets, and closes it without saving.
#   3. Prints PASS or FAIL per file and deletes the temporary folder.
# It touches nothing else: no real documents, no Office settings.
# An app that is not installed is reported as NOT_TESTED, never as a pass.

$ErrorActionPreference = 'Stop'
# The repository root is two folders up from this script.
Set-Location (Resolve-Path (Join-Path $PSScriptRoot '..\..'))
$out = Join-Path ([IO.Path]::GetTempPath()) ("lu-office-test-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $out | Out-Null
$results = @()

function Add-Result($file, $status, $detail) {
    $script:results += [pscustomobject]@{ File = $file; Status = $status; Detail = $detail }
}
function Release($o) { if ($o) { [void][Runtime.InteropServices.Marshal]::ReleaseComObject($o) } }

try {
    $env:LU_OFFICE_OUT = $out
    cargo test -p lu-core --test office -- --ignored write_samples --nocapture
    if ($LASTEXITCODE -ne 0) { throw "building the samples failed (cargo exit $LASTEXITCODE)" }

    # Word
    $file = Join-Path $out 'lu-sample.docx'
    try { $word = New-Object -ComObject Word.Application } catch { $word = $null }
    if (-not $word) { Add-Result 'lu-sample.docx' 'NOT_TESTED' 'Word is not installed' }
    else {
        try {
            $word.Visible = $false
            $word.DisplayAlerts = 0
            $word.AutomationSecurity = 3   # msoAutomationSecurityForceDisable
            # Open(FileName, ConfirmConversions, ReadOnly, AddToRecentFiles)
            $doc = $word.Documents.Open($file, $false, $true, $false)
            $text = $doc.Content.Text
            $ok = $doc.Paragraphs.Count -ge 8 -and $doc.Tables.Count -eq 1 -and $text -like '*Durable memory & checked tools <for real>.*'
            Add-Result 'lu-sample.docx' ($(if ($ok) { 'PASS' } else { 'FAIL' })) ("{0} paragraphs, {1} table" -f $doc.Paragraphs.Count, $doc.Tables.Count)
            $doc.Close(0)
        } catch { Add-Result 'lu-sample.docx' 'FAIL' $_.Exception.Message }
        finally { $word.Quit(); Release $word }
    }

    # PowerPoint (it cannot be made invisible while opening with a window,
    # so the file is opened WithWindow = false)
    $file = Join-Path $out 'lu-sample.pptx'
    try { $ppt = New-Object -ComObject PowerPoint.Application } catch { $ppt = $null }
    if (-not $ppt) { Add-Result 'lu-sample.pptx' 'NOT_TESTED' 'PowerPoint is not installed' }
    else {
        try {
            $ppt.AutomationSecurity = 3
            # Open(FileName, ReadOnly, Untitled, WithWindow)
            $deck = $ppt.Presentations.Open($file, -1, 0, 0)
            $n = $deck.Slides.Count
            $title = $deck.Slides.Item(1).Shapes.Item(1).TextFrame.TextRange.Text
            $ok = $n -eq 3 -and $title -eq 'Lu 1.0'
            Add-Result 'lu-sample.pptx' ($(if ($ok) { 'PASS' } else { 'FAIL' })) ("{0} slides, first title '{1}'" -f $n, $title)
            $deck.Close()
        } catch { Add-Result 'lu-sample.pptx' 'FAIL' $_.Exception.Message }
        finally { $ppt.Quit(); Release $ppt }
    }

    # Excel
    $file = Join-Path $out 'lu-sample.xlsx'
    try { $xl = New-Object -ComObject Excel.Application } catch { $xl = $null }
    if (-not $xl) { Add-Result 'lu-sample.xlsx' 'NOT_TESTED' 'Excel is not installed' }
    else {
        try {
            $xl.Visible = $false
            $xl.DisplayAlerts = $false
            $xl.AutomationSecurity = 3
            # Open(FileName, UpdateLinks, ReadOnly)
            $book = $xl.Workbooks.Open($file, 0, $true)
            $sheet = $book.Worksheets.Item('Budget')
            $total = $sheet.Range('B4').Value2
            $ok = $book.Worksheets.Count -eq 2 -and [double]$total -eq 1292.5
            Add-Result 'lu-sample.xlsx' ($(if ($ok) { 'PASS' } else { 'FAIL' })) ("{0} sheets, SUM formula gives {1} (expect 1292.5)" -f $book.Worksheets.Count, $total)
            $book.Close($false)
        } catch { Add-Result 'lu-sample.xlsx' 'FAIL' $_.Exception.Message }
        finally { $xl.Quit(); Release $xl }
    }
} finally {
    Remove-Item -Recurse -Force $out -ErrorAction SilentlyContinue
    [GC]::Collect()
}

$results | Format-Table -AutoSize
if ($results | Where-Object Status -eq 'FAIL') { exit 1 }
