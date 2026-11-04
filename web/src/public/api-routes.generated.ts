// Made by scripts/gen-api-docs.ts from docs/openapi.json. Do not edit by hand.
export interface ApiRoute {
  method: string;
  path: string;
  query: string[];
}

export const publicRoutes: ApiRoute[] = [
  {
    "method": "GET",
    "path": "/api/v1/bans",
    "query": [
      "limit",
      "before"
    ]
  },
  {
    "method": "GET",
    "path": "/api/v1/domains",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/links",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/links/{id}/qr.svg",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/nodes",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/nodes/{id}",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/nodes/{id}/history",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/openapi.json",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/stats/clients",
    "query": [
      "range"
    ]
  },
  {
    "method": "GET",
    "path": "/api/v1/stats/series",
    "query": [
      "range"
    ]
  },
  {
    "method": "GET",
    "path": "/api/v1/stats/summary",
    "query": [
      "range"
    ]
  },
  {
    "method": "GET",
    "path": "/install.sh",
    "query": []
  },
  {
    "method": "GET",
    "path": "/releases/{name}",
    "query": []
  }
];

export const botRoutes: ApiRoute[] = [
  {
    "method": "GET",
    "path": "/api/v1/me",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/me/nodes",
    "query": []
  },
  {
    "method": "POST",
    "path": "/api/v1/me/nodes/{id}/recheck",
    "query": []
  },
  {
    "method": "POST",
    "path": "/api/v1/me/nodes/{id}/retire",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/me/webhooks",
    "query": []
  },
  {
    "method": "POST",
    "path": "/api/v1/me/webhooks",
    "query": []
  },
  {
    "method": "DELETE",
    "path": "/api/v1/me/webhooks/{id}",
    "query": []
  },
  {
    "method": "GET",
    "path": "/api/v1/me/webhooks/{id}/deliveries",
    "query": [
      "limit",
      "before"
    ]
  },
  {
    "method": "POST",
    "path": "/api/v1/me/webhooks/{id}/test",
    "query": []
  }
];
