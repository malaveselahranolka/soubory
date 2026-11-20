// Writes THIRD-PARTY-NOTICES.txt: Phosphor Icons, then every Rust crate that goes into the shipped
// binaries (fm.exe, fm-dialogkit.exe, fm_dialog_hook.dll) or builds them, with its licence text.
// Where a crate offers a choice ("MIT OR Apache-2.0", "Unlicense OR MIT"), the MIT option is taken.
// Run after changing dependencies or the Phosphor version: node scripts/dev/gen-notices.mjs
import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..')
const SHIPPED = ['fm-app', 'fm-dialogkit', 'fm-dialog-hook']
const cargo = args => execFileSync('cargo', args, { cwd: ROOT, encoding: 'utf8', maxBuffer: 1 << 28 })

const wanted = new Set()
for (const p of SHIPPED) {
  for (const line of cargo(['tree', '-p', p, '-e', 'normal', '--prefix', 'none', '--target', 'x86_64-pc-windows-msvc']).split(/\r?\n/)) {
    const m = line.match(/^(\S+) v(\S+)/)
    if (m && !m[1].startsWith('fm')) wanted.add(`${m[1]} ${m[2]}`)
  }
}
const meta = JSON.parse(cargo(['metadata', '--format-version', '1']))
const crates = meta.packages.filter(p => wanted.has(`${p.name} ${p.version}`)).sort((a, b) => a.name.localeCompare(b.name))
if (crates.length !== wanted.size) throw new Error('some crates are missing from cargo metadata')

const read = (dir, names) => {
  const files = fs.readdirSync(dir)
  for (const n of names) {
    const f = files.find(x => x.toLowerCase() === n)
    if (f) return fs.readFileSync(path.join(dir, f), 'utf8').replace(/\r\n/g, '\n').trim()
  }
  return null
}

// Group crates that ship the same licence text (the windows-* family, crossbeam-*).
const groups = new Map()
for (const c of crates) {
  const dir = path.dirname(c.manifest_path)
  const mit = read(dir, ['license-mit', 'license-mit.md', 'license-mit.txt'])
  if (!mit) throw new Error(`no MIT licence file for ${c.name} (${c.license})`)
  const unicode = /Unicode/.test(c.license || '') ? read(dir, ['license-unicode', 'license-unicode.md']) : null
  if (/Unicode/.test(c.license || '') && !unicode) throw new Error(`no Unicode licence file for ${c.name}`)
  const text = unicode ? `${mit}\n\n${unicode}` : mit
  if (!groups.has(text)) groups.set(text, [])
  groups.get(text).push(`${c.name} ${c.version} (${c.license})`)
}

const phosphorVersion = fs.readFileSync(path.join(ROOT, 'crates', 'app', 'src', 'phosphor.rs'), 'utf8').match(/Phosphor Icons (\S+)/)[1]
const res = await fetch(`https://cdn.jsdelivr.net/npm/@phosphor-icons/core@${phosphorVersion}/LICENSE`)
if (!res.ok) throw new Error(`Phosphor licence: ${res.status}`)
const phosphor = (await res.text()).replace(/\r\n/g, '\n').trim()

const rule = '-'.repeat(78)
let out = `Third-party notices for Soubory (fm)

Soubory itself is licensed under the GNU General Public License v3.0 or later (see LICENSE).
It includes the following third-party works under their own licences.

${rule}
Phosphor Icons ${phosphorVersion} (https://phosphoricons.com, @phosphor-icons/core)
Sidebar, toolbar and file-type icons, embedded as SVG path data.

${phosphor}
`
for (const [text, names] of groups) {
  const terms = /UNICODE LICENSE/i.test(text) ? 'the MIT License and, for their Unicode data, the Unicode License v3' : 'the MIT License'
  out += `\n${rule}\nRust crates, used under ${terms}:\n${names.map(n => `  ${n}`).join('\n')}\n\n${text}\n`
}
fs.writeFileSync(path.join(ROOT, 'THIRD-PARTY-NOTICES.txt'), out.replace(/\n/g, '\r\n'))
console.log(`THIRD-PARTY-NOTICES.txt: Phosphor ${phosphorVersion} and ${crates.length} crates in ${groups.size} licence groups`)
