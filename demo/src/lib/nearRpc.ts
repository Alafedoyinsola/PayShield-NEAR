const RPC_URL = '/near-rpc'
export const CONTRACT_ID = 'payshield-agent-demo.testnet'

interface RpcResponse {
  result?: {
    result?: number[]
    amount?: string
    block_height?: number
  }
  error?: {
    message?: string
  }
}

export async function viewContract<T>(
  methodName: string,
  args: Record<string, unknown> = {},
): Promise<T> {
  const response = await fetch(RPC_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: `payshield-${methodName}`,
      method: 'query',
      params: {
        request_type: 'call_function',
        finality: 'final',
        account_id: CONTRACT_ID,
        method_name: methodName,
        args_base64: btoa(JSON.stringify(args)),
      },
    }),
  })

  if (!response.ok) {
    throw new Error(`NEAR RPC HTTP error: ${response.status}`)
  }

  const data = (await response.json()) as RpcResponse

  if (data.error) {
    throw new Error(data.error.message ?? 'NEAR RPC request failed')
  }

  if (!data.result?.result) {
    throw new Error(`No result returned by ${methodName}`)
  }

  const decoded = new TextDecoder().decode(
    new Uint8Array(data.result.result),
  )

  return JSON.parse(decoded) as T
}

export async function getAccountBalance(): Promise<string> {
  const response = await fetch(RPC_URL, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({
      jsonrpc: '2.0',
      id: 'payshield-account-balance',
      method: 'query',
      params: {
        request_type: 'view_account',
        finality: 'final',
        account_id: CONTRACT_ID,
      },
    }),
  })

  if (!response.ok) {
    throw new Error(`NEAR RPC HTTP error: ${response.status}`)
  }

  const data = (await response.json()) as RpcResponse

  if (data.error || !data.result?.amount) {
    throw new Error(data.error?.message ?? 'Could not load contract balance')
  }

  return data.result.amount
}
