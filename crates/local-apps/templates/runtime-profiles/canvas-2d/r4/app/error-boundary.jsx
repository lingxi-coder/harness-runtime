import React from "react";

/// Deliberately built from plain elements and Ionic CSS VARIABLES only — no
/// `ion-*` components.
///
/// This boundary sits OUTSIDE `IonApp`, because one of the failures it has to
/// survive is Ionic itself failing to initialize. A fallback that renders
/// `IonButton` would throw again from inside the handler and leave a blank
/// screen with nothing in the log to explain it.
export class ErrorBoundary extends React.Component {
  constructor(props) {
    super(props);
    this.state = { error: null };
  }

  static getDerivedStateFromError(error) {
    return { error };
  }

  render() {
    if (!this.state.error) return this.props.children;

    return (
      <main
        style={{
          minHeight: "100svh",
          display: "grid",
          placeItems: "center",
          padding: "1.5rem",
          paddingTop: "calc(1.5rem + var(--safe-area-top, 0px))",
          background: "var(--ion-background-color, #fff)",
          color: "var(--ion-text-color, #000)",
          font: "1rem/1.5 var(--platform-font, system-ui, sans-serif)",
        }}
      >
        <section style={{ maxWidth: "28rem", width: "100%" }}>
          <p style={{ margin: 0, color: "var(--ion-color-danger, #c5000f)", fontWeight: 600 }}>
            应用暂时无法显示
          </p>
          <h1 style={{ margin: "0.5rem 0 0", fontSize: "1.5rem" }}>出现了意外错误</h1>
          <p style={{ marginTop: "0.75rem", opacity: 0.75 }}>
            {this.state.error.message || "请重新载入本地应用。"}
          </p>
          <button
            type="button"
            onClick={() => window.location.reload()}
            style={{
              marginTop: "1.5rem",
              minHeight: "var(--platform-control-min, 44px)",
              padding: "0 1.25rem",
              borderRadius: "0.5rem",
              border: "none",
              font: "inherit",
              fontWeight: 600,
              color: "var(--ion-color-primary-contrast, #fff)",
              background: "var(--ion-color-primary, #0054e9)",
            }}
          >
            重新载入
          </button>
        </section>
      </main>
    );
  }
}
