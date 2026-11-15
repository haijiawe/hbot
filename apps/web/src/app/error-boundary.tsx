/**
 * 错误边界：每个 feature 外层都有一层，异常不退化成白屏（AGENTS.md §6）。
 *
 * 上报走 console.error（M1 接核心日志），用户可以选择"重试渲染"或"重新加载"。
 */

import { Component, type ErrorInfo, type ReactNode } from 'react'

interface ErrorBoundaryProps {
  name: string
  children: ReactNode
}

interface ErrorBoundaryState {
  error: Error | null
}

export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  override state: ErrorBoundaryState = { error: null }

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error }
  }

  override componentDidCatch(error: Error, info: ErrorInfo): void {
    // 显式上报，不静默吞掉（AGENTS.md §0.8）。
    console.error(`[${this.props.name}] 渲染失败`, error, info.componentStack)
  }

  private readonly handleRetry = (): void => {
    this.setState({ error: null })
  }

  private readonly handleReload = (): void => {
    window.location.reload()
  }

  override render(): ReactNode {
    const { error } = this.state
    if (!error) return this.props.children

    return (
      <div className="boundary" role="alert">
        <h2 className="boundary__title">{this.props.name} 渲染失败</h2>
        <p className="boundary__message">{error.message}</p>
        <div className="boundary__actions">
          <button type="button" className="button" onClick={this.handleRetry} aria-label="重试渲染">
            重试
          </button>
          <button type="button" className="button" onClick={this.handleReload} aria-label="重新加载窗口">
            重新加载
          </button>
        </div>
      </div>
    )
  }
}