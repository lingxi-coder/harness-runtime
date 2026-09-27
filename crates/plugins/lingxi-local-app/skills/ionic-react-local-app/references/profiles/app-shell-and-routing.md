# App shell and routing

- Keep routed screens inside `IonRouterOutlet` and wrap routed views in
  `IonPage`.
- Keep the checked-in router flavor and do not swap in a parallel navigation
  stack.
- Import `useIonRouter` and every other Ionic hook from `@ionic/react`, never
  from `@ionic/react-router`.
- Use one intentional navigation model per target presentation instead of
  mixing arbitrary tabs, stacks, and sheets.
- Let overlays, sheets, and modals stay Ionic surfaces so focus and dismissal
  behavior remain platform-correct.
- Keep safe-area handling attached to the provider-published custom properties.
- When a canvas or Three scene is present, keep the scene under one surface and
  keep HUD, pause, and settings in separate overlay UI.
- Import Ionic hooks and components from `@ionic/react`, not from
  `@ionic/react-router`.

## Sources

Reviewed: 2026-08-27

- Ionic React navigation: https://ionicframework.com/docs/react/navigation
- IonPage navigation guidance: https://ionicframework.com/docs/react/navigation#ionpage
