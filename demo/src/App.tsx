import { useCallback, useEffect, useState } from 'react'
import { nearConnector } from './lib/nearConnector'
import { CONTRACT_ID, getAccountBalance, viewContract } from './lib/nearRpc'
import './App.css'

interface PaymentPolicy {
  max_amount_per_tx: string
  daily_spending_limit: string
  require_approval_above: string
}

interface DailySpending {
  day_id: number
  confirmed_amount: string
  reserved_amount: string
}

interface PaymentProposal {
  proposal_id: number
  recipient: string
  amount: string
  reason: string
  created_at: number
  expires_at: number
  status: string
}

interface PaymentRecord {
  payment_id: number
  recipient: string
  amount: string
  accounting_day: number
  proposal_id: number | null
  initiated_at: number
  settled_at: number | null
  status: string
  settlement_source: unknown
  reconciliation_evidence: string | null
}

interface DashboardData {
  policy: PaymentPolicy
  owner: string
  agent: string
  nextProposalId: number
  nextPaymentId: number
  currentDay: number
  spending: DailySpending
  balance: string
  pendingProposals: number
  proposals: PaymentProposal[]
  payments: PaymentRecord[]
}

const YOCTO = 10n ** 24n

function formatNear(value: string): string {
  const amount = BigInt(value)
  const whole = amount / YOCTO
  const fraction = amount % YOCTO

  if (fraction === 0n) return whole.toString()

  return `${whole}.${fraction.toString().padStart(24, '0').replace(/0+$/, '').slice(0, 4)}`
}

function App() {
  const [account, setAccount] = useState('')
  const [connecting, setConnecting] = useState(false)
  const [error, setError] = useState('')
  const [data, setData] = useState<DashboardData | null>(null)
  const [loading, setLoading] = useState(true)

  const [theme, setTheme] = useState<'dark' | 'light'>(() => {
    try {
      return window.localStorage.getItem('payshield-theme') === 'light'
        ? 'light'
        : 'dark'
    } catch {
      return 'dark'
    }
  })

  useEffect(() => {
    try {
      window.localStorage.setItem('payshield-theme', theme)
    } catch {
      // Theme remains usable if browser storage is unavailable.
    }
  }, [theme])



  const loadDashboard = useCallback(async () => {
    setLoading(true)
    setError('')

    try {
      const [
        policy,
        owner,
        agent,
        nextProposalId,
        nextPaymentId,
        currentDay,
        balance,
      ] = await Promise.all([
        viewContract<PaymentPolicy>('get_policy'),
        viewContract<string>('get_owner'),
        viewContract<string>('get_agent'),
        viewContract<number>('get_next_proposal_id'),
        viewContract<number>('get_next_payment_id'),
        viewContract<number>('get_current_day_index'),
        getAccountBalance(),
      ])

      const spending = await viewContract<DailySpending>(
        'get_daily_spending',
        { day_id: currentDay },
      )

      const proposalIds = Array.from(
        { length: Math.max(0, nextProposalId - 1) },
        (_, index) => index + 1,
      )

      const paymentIds = Array.from(
        { length: Math.max(0, nextPaymentId - 1) },
        (_, index) => index + 1,
      )

      const [proposalResults, paymentResults] = await Promise.all([
        Promise.all(
          proposalIds.map((proposalId) =>
            viewContract<PaymentProposal | null>('get_proposal', {
              proposal_id: proposalId,
            }),
          ),
        ),
        Promise.all(
          paymentIds.map((paymentId) =>
            viewContract<PaymentRecord | null>('get_payment', {
              payment_id: paymentId,
            }),
          ),
        ),
      ])

      const proposals = proposalResults.filter(
        (proposal): proposal is PaymentProposal => proposal !== null,
      )

      const payments = paymentResults.filter(
        (payment): payment is PaymentRecord => payment !== null,
      )

      const pendingProposals = proposals.filter(
        (proposal) => proposal.status === 'Pending',
      ).length

      setData({
        policy,
        owner,
        agent,
        nextProposalId,
        nextPaymentId,
        currentDay,
        spending,
        balance,
        pendingProposals,
        proposals,
        payments,
      })
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Could not load contract data')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void loadDashboard()
  }, [loadDashboard])

  async function connectWallet() {
    setConnecting(true)
    setError('')

    try {
      await nearConnector.connect()
      const { accounts } = await nearConnector.getConnectedWallet()
      setAccount(accounts[0]?.accountId ?? '')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Wallet connection failed')
    } finally {
      setConnecting(false)
    }
  }

  async function disconnectWallet() {
    setError('')

    try {
      await nearConnector.disconnect()
      setAccount('')
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Wallet disconnection failed')
    }
  }

  return (
    <main className="app-shell" data-theme={theme}>
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">P</div>
          <div>
            <strong>PayShield</strong>
            <span>PAYMENT SECURITY</span>
          </div>
        </div>

        <div className="sidebar-label">WORKSPACE</div>
        <nav>
          <a className="nav-item active" href="#overview">▦ <span>Overview</span></a>
          <a className="nav-item" href="#payments">↗ <span>Payments</span></a>
          <a className="nav-item" href="#approvals">✓ <span>Approvals</span></a>
          <a className="nav-item" href="#activity">◷ <span>Activity</span></a>
        </nav>

        <div className="sidebar-bottom">
          <div className="network-card">
            <span className="status-dot" />
            <div>
              <strong>NEAR Testnet</strong>
              <small>Connected to network</small>
            </div>
          </div>
          <span className="version">PAYSHIELD DEMO · 0.1.0</span>
        </div>
      </aside>

      <section className="main-panel" id="overview">
        <header className="topbar">
          <div className="breadcrumb">Workspace <span>/</span> Overview</div>
          <button
            className="theme-toggle"
            type="button"
            onClick={() => setTheme((current) => current === 'dark' ? 'light' : 'dark')}
            aria-label={theme === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'}
          >
            {theme === 'dark' ? '☀ Light mode' : '☾ Dark mode'}
          </button>
          {account ? (
            <button className="wallet-button connected" onClick={disconnectWallet}>
              <span className="status-dot" />
              {account}
              <span className="disconnect-label">Disconnect</span>
            </button>
          ) : (
            <button
              className="wallet-button"
              onClick={connectWallet}
              disabled={connecting}
            >
              {connecting ? 'Connecting...' : 'Connect wallet ↗'}
            </button>
          )}
        </header>

        <div className="content">
          <section className="welcome">
            <div>
              <div className="eyebrow">PAYMENT CONTROL CENTER</div>
              <h1>Overview</h1>
              <p>Monitor payment activity, spending limits, and approval controls.</p>
            </div>
            <div className="security-badge">
              <span className="status-dot" />
              Policy protection enabled
            </div>
          </section>

          {error && <div className="error-message" role="alert">{error}</div>}

          <section className="metrics-grid" aria-label="Payment metrics">
            <article className="metric-card">
              <div className="metric-heading">VAULT BALANCE <span>◇</span></div>
              <div className="metric-value">
                {loading ? '...' : data ? formatNear(data.balance) : '—'} <small>NEAR</small>
              </div>
              <p>{data ? 'Contract account balance' : loading ? 'Loading contract data' : 'Unavailable'}</p>
            </article>
            <article className="metric-card">
              <div className="metric-heading">DAILY SPENDING LIMIT <span>⌁</span></div>
              <div className="metric-value">
                {data ? formatNear(data.policy.daily_spending_limit) : '—'} <small>NEAR</small>
              </div>
              <p>On-chain policy limit</p>
            </article>
            <article className="metric-card">
              <div className="metric-heading">MAXIMUM PER PAYMENT <span>↗</span></div>
              <div className="metric-value">
                {data ? formatNear(data.policy.max_amount_per_tx) : '—'} <small>NEAR</small>
              </div>
              <p>On-chain policy limit</p>
            </article>
            <article className="metric-card">
              <div className="metric-heading">PENDING APPROVAL <span>◷</span></div>
              <div className="metric-value">
                {data ? data.pendingProposals : '—'}
              </div>
              <p>{data ? 'Pending proposals' : 'Awaiting contract data'}</p>
            </article>
          </section>

          <section className="dashboard-grid">
            <article className="panel contract-panel">
              <div className="panel-header">
                <div>
                  <div className="eyebrow">SMART CONTRACT</div>
                  <h2>Contract connection</h2>
                </div>
                <span className="tag">TESTNET</span>
              </div>
              <div className="contract-details">
                <div>
                  <span>Contract account</span>
                  <strong>{CONTRACT_ID}</strong>
                </div>
                <div>
                  <span>Owner account</span>
                  <strong>{data?.owner ?? 'Loading...'}</strong>
                </div>
                <div>
                  <span>Agent account</span>
                  <strong>{data?.agent ?? 'Loading...'}</strong>
                </div>
                <div>
                  <span>Connected wallet</span>
                  <strong>{account || 'No wallet connected'}</strong>
                </div>
                <div>
                  <span>Contract data</span>
                  <strong className="muted-value">
                    {loading ? 'Loading...' : data ? 'Read successfully' : 'Unavailable'}
                  </strong>
                </div>
              </div>
            </article>

            <article className="panel approval-panel" id="approvals">
              <div className="panel-header">
                <div>
                  <div className="eyebrow">OWNER CONTROL</div>
                  <h2>Approval threshold</h2>
                </div>
                <span className="threshold-icon">✓</span>
              </div>
              <div className="threshold-value">
                {data ? formatNear(data.policy.require_approval_above) : '—'} <small>NEAR</small>
              </div>
              <p className="panel-description">
                Payments above this amount require owner approval before execution.
              </p>
              <div className="rule-note">
                <span className="status-dot" />
                Approval and execution are separate steps.
              </div>
            </article>
          </section>

          <section className="panel activity-panel" id="activity">
            <div className="panel-header">
              <div>
                <div className="eyebrow">TRANSACTION MONITOR</div>
                <h2>On-chain usage</h2>
              </div>
              <button className="wallet-button" onClick={() => void loadDashboard()} disabled={loading}>
                {loading ? 'Refreshing...' : '↻ Refresh'}
              </button>
            </div>
            <div className="contract-details usage-details">
              <div>
                <span>Current UTC accounting day</span>
                <strong>{data?.currentDay ?? '—'}</strong>
              </div>
              <div>
                <span>Confirmed spending today</span>
                <strong>{data ? `${formatNear(data.spending.confirmed_amount)} NEAR` : '—'}</strong>
              </div>
              <div>
                <span>Reserved spending today</span>
                <strong>{data ? `${formatNear(data.spending.reserved_amount)} NEAR` : '—'}</strong>
              </div>
              <div>
                <span>Next proposal ID</span>
                <strong>{data?.nextProposalId ?? '—'}</strong>
              </div>
              <div>
                <span>Next payment ID</span>
                <strong>{data?.nextPaymentId ?? '—'}</strong>
              </div>
            </div>
          </section>

          <section className="panel payments-panel" id="payments">
            <div className="panel-header">
              <div>
                <div className="eyebrow">PAYMENT RECORDS</div>
                <h2>Payments</h2>
                <p className="panel-description">
                  On-chain payment records and their settlement status.
                </p>
              </div>
              <span className="tag">
                {data ? `${data.payments.length} RECORDS` : 'LOADING'}
              </span>
            </div>

            {!data ? (
              <div className="empty-state">
                <strong>{loading ? 'Loading payments...' : 'Payment records unavailable'}</strong>
                <p>{loading ? 'Reading payment records from NEAR Testnet.' : 'Refresh the dashboard to try again.'}</p>
              </div>
            ) : data.payments.length === 0 ? (
              <div className="empty-state">
                <div className="empty-icon">↗</div>
                <strong>No payments recorded yet</strong>
                <p>Completed and in-flight payment records will appear here.</p>
              </div>
            ) : (
              <div className="payment-list">
                {data.payments.map((payment) => (
                  <article className="payment-entry" key={payment.payment_id}>
                    <div className="payment-entry-main">
                      <div>
                        <strong>Payment #{payment.payment_id}</strong>
                        <span>{payment.recipient}</span>
                      </div>
                      <div className="payment-amount">
                        {formatNear(payment.amount)} <small>NEAR</small>
                      </div>
                    </div>
                    <div className="payment-entry-meta">
                      <span>
                        Status: <strong>{payment.status}</strong>
                      </span>
                      <span>
                        Accounting day: <strong>{payment.accounting_day}</strong>
                      </span>
                      <span>
                        Proposal: <strong>{payment.proposal_id ?? 'Direct payment'}</strong>
                      </span>
                      <span>
                        Initiated: <strong>{new Date(payment.initiated_at / 1_000_000).toLocaleString()}</strong>
                      </span>
                      <span>
                        Settled: <strong>{payment.settled_at ? new Date(payment.settled_at / 1_000_000).toLocaleString() : 'Not settled'}</strong>
                      </span>
                    </div>
                  </article>
                ))}
              </div>
            )}
          </section>

          <footer className="footer">
            <span>PayShield · Built on NEAR Protocol</span>
            <span>Testnet environment · Read-only contract data</span>
          </footer>
        </div>
      </section>
    </main>
  )
}

export default App
