// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! [Router] in client

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use ceresdbproto::storage::{self, RouteRequest};
use dashmap::DashMap;

use crate::{
    errors::Result,
    model::route::Endpoint,
    rpc_client::{RpcClient, RpcContext},
    Error,
};

/// Used to route tables to endpoints.
#[async_trait]
pub trait Router: Send + Sync {
    async fn route(&self, tables: &[String], ctx: &RpcContext) -> Result<Vec<Option<Endpoint>>>;

    fn evict(&self, tables: &[String]);
}

/// Implementation for [`Router`].
///
/// There is cache in [`RouterImpl`], it will return endpoints in cache first.
/// If returned endpoints is outdated, you should call [`evict`] to remove them.
/// And [`RouterImpl`] will fetch new endpoints when you call ['route'] again.
///
/// [`route`]: RouterImpl::route
/// [`evict`]: RouterImpl::evict
pub struct RouterImpl {
    default_endpoint: Endpoint,
    cache: DashMap<String, Endpoint>,
    rpc_client: Arc<dyn RpcClient>,
}

impl RouterImpl {
    pub fn new(default_endpoint: Endpoint, rpc_client: Arc<dyn RpcClient>) -> Self {
        Self {
            default_endpoint,
            cache: DashMap::new(),
            rpc_client,
        }
    }
}

#[async_trait]
impl Router for RouterImpl {
    async fn route(&self, tables: &[String], ctx: &RpcContext) -> Result<Vec<Option<Endpoint>>> {
        assert!(ctx.database.is_some());

        let mut target_endpoints = vec![Some(self.default_endpoint.clone()); tables.len()];

        // Find from cache firstly and collect misses.
        let misses = {
            let mut misses = HashMap::new();
            for (idx, table) in tables.iter().enumerate() {
                match self.cache.get(table) {
                    Some(pair) => {
                        target_endpoints[idx] = Some(pair.value().clone());
                    }

                    None => {
                        misses.insert(table.clone(), idx);
                    }
                }
            }
            misses
        };

        // Get endpoints of misses from remote.
        let req_ctx = storage::RequestContext {
            database: ctx.database.clone().unwrap(),
        };
        let miss_tables = misses.keys().cloned().collect();
        let req = RouteRequest {
            context: Some(req_ctx),
            tables: miss_tables,
        };
        let resp = self.rpc_client.route(ctx, req).await?;

        // Fill miss endpoint and update cache.
        for route in resp.routes {
            // Endpoint may be none, and not cache it when it is none.
            if route.endpoint.is_none() {
                continue;
            }

            // Impossible to get none.
            let idx = misses.get(&route.table).ok_or_else(|| {
                Error::Unknown(format!("Unknown table:{} in response", route.table))
            })?;
            let endpoint: Endpoint = route.endpoint.unwrap().into();
            self.cache.insert(route.table, endpoint.clone());
            target_endpoints[*idx] = Some(endpoint);
        }

        Ok(target_endpoints)
    }

    fn evict(&self, tables: &[String]) {
        tables.iter().for_each(|e| {
            self.cache.remove(e.as_str());
        })
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use dashmap::DashMap;

    use super::{Router, RouterImpl};
    use crate::{
        model::route::Endpoint,
        rpc_client::{MockRpcClient, RpcContext},
    };

    #[tokio::test]
    async fn test_basic_flow() {
        // Init mock route table
        let table1 = "table1".to_string();
        let table2 = "table2".to_string();
        let table3 = "table3".to_string();
        let table4 = "table4".to_string();
        let endpoint1 = Endpoint::new("192.168.0.1".to_string(), 11);
        let endpoint2 = Endpoint::new("192.168.0.2".to_string(), 12);
        let endpoint3 = Endpoint::new("192.168.0.3".to_string(), 13);
        let endpoint4 = Endpoint::new("192.168.0.4".to_string(), 14);
        let default_endpoint = Endpoint::new("192.168.0.5".to_string(), 15);

        // Init mock client with route1 and route2
        let route_table = Arc::new(DashMap::default());
        let mock_rpc_client = MockRpcClient {
            route_table: route_table.clone(),
        };
        mock_rpc_client
            .route_table
            .insert(table1.clone(), endpoint1.clone());
        mock_rpc_client
            .route_table
            .insert(table2.clone(), endpoint2.clone());

        // Follow these steps to check wether cache is used or not:
        // route --> change route_table --> route again.
        let ctx = RpcContext {
            database: Some("db".to_string()),
            timeout: None,
        };
        let tables = vec![table1.clone(), table2.clone()];
        let route_client = RouterImpl::new(default_endpoint.clone(), Arc::new(mock_rpc_client));
        let route_res1 = route_client.route(&tables, &ctx).await.unwrap();
        assert_eq!(&endpoint1, route_res1.get(0).unwrap().as_ref().unwrap());
        assert_eq!(&endpoint2, route_res1.get(1).unwrap().as_ref().unwrap());

        route_table.insert(table1.clone(), endpoint3.clone());
        route_table.insert(table2.clone(), endpoint4.clone());

        let route_res2 = route_client.route(&tables, &ctx).await.unwrap();
        assert_eq!(&endpoint1, route_res2.get(0).unwrap().as_ref().unwrap());
        assert_eq!(&endpoint2, route_res2.get(1).unwrap().as_ref().unwrap());

        route_client.evict(&[table1.clone(), table2.clone()]);

        let route_res3 = route_client.route(&tables, &ctx).await.unwrap();
        assert_eq!(&endpoint3, route_res3.get(0).unwrap().as_ref().unwrap());
        assert_eq!(&endpoint4, route_res3.get(1).unwrap().as_ref().unwrap());

        let route_res4 = route_client.route(&[table3, table4], &ctx).await.unwrap();
        assert_eq!(
            &default_endpoint,
            route_res4.get(0).unwrap().as_ref().unwrap()
        );
        assert_eq!(
            &default_endpoint,
            route_res4.get(1).unwrap().as_ref().unwrap()
        );
    }
}
