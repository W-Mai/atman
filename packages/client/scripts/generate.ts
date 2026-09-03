import { compile } from 'json-schema-to-typescript'

const packageRoot = new URL('../', import.meta.url)
const workspaceRoot = new URL('../../../', import.meta.url)
const schemaUrl = new URL('crates/atman-proto/schema/protocol.schema.json', workspaceRoot)
const manifestUrl = new URL('crates/atman-proto/schema/method-manifest.json', workspaceRoot)
const typesUrl = new URL('src/generated/types.generated.ts', packageRoot)
const methodsUrl = new URL('src/generated/methods.generated.ts', packageRoot)
const check = process.argv.includes('--check')

type PayloadSchema = {
  rust_type: string
  schema: { $ref: string }
}

type Method = {
  name: string
  kind: 'command' | 'query'
  revision: number
  params: PayloadSchema
  result: PayloadSchema
}

type Manifest = {
  protocol_version: number
  snapshot_schema_version: number
  event_schema_version: number
  methods: Method[]
}

const schema = await Bun.file(schemaUrl).json()
const manifest = (await Bun.file(manifestUrl).json()) as Manifest
const generatedTypes = await compile(schema, 'ProtocolPayload', {
  bannerComment: '// Generated from atman-proto. Do not edit.\n',
  enableConstEnums: false,
  style: {
    bracketSpacing: true,
    printWidth: 100,
    semi: false,
    singleQuote: true,
    trailingComma: 'all',
  },
  unreachableDefinitions: true,
})
const generatedMethods = renderMethods(manifest)

await writeOrCheck(typesUrl, generatedTypes)
await writeOrCheck(methodsUrl, generatedMethods)

function renderMethods(manifest: Manifest): string {
  const types = new Set<string>()
  for (const method of manifest.methods) {
    types.add(referenceName(method.params.schema.$ref))
    types.add(referenceName(method.result.schema.$ref))
  }
  const imports = [...types].sort().join(',\n  ')
  const entries = manifest.methods
    .map((method) => {
      const params = referenceName(method.params.schema.$ref)
      const result = referenceName(method.result.schema.$ref)
      return `  '${method.name}': {\n    kind: '${method.kind}'\n    revision: ${method.revision}\n    params: ${params}\n    result: ${result}\n  }`
    })
    .join('\n')
  const runtimeEntries = manifest.methods
    .map(
      (method) =>
        `  '${method.name}': { kind: '${method.kind}', revision: ${method.revision} },`,
    )
    .join('\n')

  return `// Generated from atman-proto. Do not edit.\n\nimport type {\n  ${imports},\n} from './types.generated'\n\nexport const PROTOCOL_VERSION = ${manifest.protocol_version} as const\nexport const SNAPSHOT_SCHEMA_VERSION = ${manifest.snapshot_schema_version} as const\nexport const EVENT_SCHEMA_VERSION = ${manifest.event_schema_version} as const\n\nexport interface RpcMethodMap {\n${entries}\n}\n\nexport type RpcMethodName = keyof RpcMethodMap\nexport type RpcMethodParams<M extends RpcMethodName> = RpcMethodMap[M]['params']\nexport type RpcMethodResult<M extends RpcMethodName> = RpcMethodMap[M]['result']\n\nexport const RPC_METHODS = {\n${runtimeEntries}\n} as const satisfies Record<RpcMethodName, { kind: 'command' | 'query'; revision: number }>\n`
}

function referenceName(reference: string): string {
  const prefix = '#/$defs/'
  if (!reference.startsWith(prefix)) {
    throw new Error(`unsupported protocol schema reference: ${reference}`)
  }
  const name = reference.slice(prefix.length)
  if (!/^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name)) {
    throw new Error(`protocol schema name is not a TypeScript identifier: ${name}`)
  }
  return name
}

async function writeOrCheck(url: URL, expected: string): Promise<void> {
  const file = Bun.file(url)
  if (check) {
    if (!(await file.exists()) || (await file.text()) !== expected) {
      throw new Error(`${url.pathname} is stale; run \`bun run protocol:generate\``)
    }
    return
  }
  if ((await file.exists()) && (await file.text()) === expected) {
    return
  }
  await Bun.write(url, expected)
}
