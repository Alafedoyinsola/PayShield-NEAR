import { NearConnector } from '@hot-labs/near-connect'

export const nearConnector = new NearConnector({
  network: 'testnet',
  autoConnect: false,
})
