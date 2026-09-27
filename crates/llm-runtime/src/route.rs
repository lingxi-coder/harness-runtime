use crate::{ProtocolFamily, ResolvedRoute};

#[derive(Debug)]
pub struct Route {
    pub resolved_route: ResolvedRoute,
    pub protocol: ProtocolFamily,
}
