import {
  IonButton,
  IonContent,
  IonHeader,
  IonItem,
  IonLabel,
  IonList,
  IonNote,
  IonPage,
  IonTitle,
  IonToolbar,
} from "@ionic/react";
import { useLingXi } from "@/lib/lingxi-provider";
import { useAppStore } from "@/src/stores/app-store";

/// The default editable entry point. Replace its content freely; keep the
/// shape:
///   - `IonPage` as the ROOT element, or the router outlet cannot animate it
///   - `IonHeader`/`IonToolbar` for the title bar, `IonContent` for the body
///   - `routerLink` (not an onClick + navigate) so the platform back gesture and
///     the transition direction come out right
export function HomeScreen() {
  const { adapter, bridgeReady } = useLingXi();
  const completedActions = useAppStore((state) => state.completedActions);
  const completeAction = useAppStore((state) => state.completeAction);

  return (
    <IonPage>
      <IonHeader>
        <IonToolbar>
          <IonTitle>概览</IonTitle>
        </IonToolbar>
      </IonHeader>

      <IonContent className="ion-padding">
        <IonList inset>
          <IonItem>
            <IonLabel>
              <h2>平台</h2>
              <p>
                {adapter.ionicMode === "ios" ? "iOS 外观" : "Material 外观"} ·{" "}
                {adapter.key}
              </p>
            </IonLabel>
          </IonItem>
          <IonItem>
            <IonLabel>
              <h2>原生桥接</h2>
              <p>{bridgeReady ? "已连接" : "未连接"}</p>
            </IonLabel>
            <IonNote slot="end">{completedActions}</IonNote>
          </IonItem>
          <IonItem button detail routerLink="/detail">
            <IonLabel>查看详情</IonLabel>
          </IonItem>
        </IonList>

        <IonButton expand="block" onClick={completeAction}>
          记录一次操作
        </IonButton>
      </IonContent>
    </IonPage>
  );
}
