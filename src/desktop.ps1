$ErrorActionPreference = 'Stop'
$utf8 = New-Object System.Text.UTF8Encoding($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8

function Read-HierarchyNode($id, $depth, $kind) {
    $xml = ''
    $app.GetHierarchy($id, $depth, [ref]$xml, 2)
    $node = ([xml]$xml).DocumentElement
    if ($node.LocalName -cne $kind -or $node.GetAttribute('ID') -cne $id) {
        throw "The OneNote desktop $kind ID did not match the requested object."
    }
    return $node
}

function Read-Parent($id) {
    $parent = ''
    $app.GetHierarchyParent($id, [ref]$parent)
    if (!$parent) { throw 'OneNote did not return a parent ID.' }
    return $parent
}

function Get-PageLevel($page) {
    if (!$page.HasAttribute('pageLevel')) { return 0 }
    return ([int]$page.GetAttribute('pageLevel')) - 1
}

try {
    $request = [Console]::In.ReadToEnd() | ConvertFrom-Json
    $id = $request.id
    $app = New-Object -ComObject OneNote.Application
    $parent = Read-Parent $id

    if ($request.operation -eq 'rename') {
        $section = Read-HierarchyNode $id 0 'Section'
        $previous = $section.GetAttribute('name')
        $changed = $previous -cne $request.name
        if ($changed) {
            $section.SetAttribute('name', [string]$request.name)
            $app.UpdateHierarchy($section.OuterXml, 2)
        }
        $after = Read-HierarchyNode $id 0 'Section'
        if ((Read-Parent $id) -cne $parent -or $after.GetAttribute('name') -cne $request.name) {
            throw 'Read-back did not confirm the new section name and original parent.'
        }
        $result = @{ ok = $true; section_id = $id; parent_id = $parent; name = $request.name; changed = $changed }
    } else {
        $page = Read-HierarchyNode $id 0 'Page'
        $level = [int]$request.level
        $previous = Get-PageLevel $page
        $changed = $previous -ne $level
        if ($changed) {
            $edit = $page.CloneNode($false)
            $edit.RemoveAllAttributes()
            $edit.SetAttribute('ID', $id)
            $edit.SetAttribute('pageLevel', [string]($level + 1))
            $app.UpdateHierarchy($edit.OuterXml, 2)
            $after = Read-HierarchyNode $id 0 'Page'
            if ((Read-Parent $id) -cne $parent -or (Get-PageLevel $after) -ne $level) {
                throw 'Read-back did not confirm the new page level and the original section.'
            }
        }
        $result = @{ ok = $true; page_id = $id; section_id = $parent; level = $level; changed = $changed }
    }
    [Console]::Out.WriteLine(($result | ConvertTo-Json -Compress))
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
