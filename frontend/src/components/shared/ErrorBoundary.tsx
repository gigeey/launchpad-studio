import { Component, type ReactNode, type ErrorInfo } from "react";

interface Props {
  children: ReactNode;
  fallback?: ReactNode;
}

interface State {
  hasError: boolean;
  error: Error | null;
}

export class ErrorBoundary extends Component<Props, State> {
  state: State = { hasError: false, error: null };

  static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("[ErrorBoundary] render error:", error, info);
  }

  render() {
    if (this.state.hasError) {
      if (this.props.fallback != null) return this.props.fallback;
      return (
        <div className="flex flex-1 items-center justify-center p-8 text-center">
          <div className="max-w-sm">
            <p className="text-[14px] font-semibold text-[var(--text-primary)] mb-1">
              Something went wrong
            </p>
            <p className="text-[12px] text-[var(--text-tertiary)]">
              {this.state.error?.message ?? "An unexpected error occurred."}
            </p>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
