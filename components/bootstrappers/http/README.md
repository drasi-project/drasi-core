# HTTP Bootstrap Plugin for Drasi

Fetches initial state from REST APIs to populate Drasi continuous queries. Supports multiple endpoints, various authentication methods, pagination strategies, and flexible response-to-graph-element mapping.

## Features

- **Multiple endpoints** – fetch nodes and relations from different APIs in a single bootstrap
- **5 pagination strategies** – offset/limit, page number, cursor-based, Link header, next-URL
- **4 authentication methods** – Bearer token, API key, Basic auth, OAuth2 client credentials
- **Flexible mapping** – Handlebars templates transform any JSON/XML/YAML response into graph elements
- **Retry with backoff** – configurable retries for transient failures
- **Streaming** – emits elements as they're parsed, no full-dataset buffering

## Configuration Reference

### Top-Level Config

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `endpoints` | array | (required) | List of endpoint configurations |
| `timeoutSeconds` | integer | 30 | HTTP request timeout in seconds |
| `maxRetries` | integer | 3 | Maximum retry attempts on failure |
| `retryDelayMs` | integer | 1000 | Base delay between retries (multiplied by attempt number) |

### Endpoint Config

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `url` | string | (required) | The URL to fetch data from |
| `method` | string | `"GET"` | HTTP method (`GET`, `POST`, `PUT`) |
| `headers` | object | `{}` | Additional HTTP headers |
| `body` | object | null | Request body (for POST/PUT) |
| `auth` | object | null | Authentication configuration |
| `pagination` | object | null | Pagination configuration |
| `response` | object | (required) | Response parsing and mapping |

### Authentication

#### Bearer Token
```json
{
  "type": "bearer",
  "token_env": "MY_API_TOKEN"
}
```

#### API Key (Header or Query)
```json
{
  "type": "api_key",
  "location": "header",
  "name": "X-API-Key",
  "value_env": "MY_API_KEY"
}
```

```json
{
  "type": "api_key",
  "location": "query",
  "name": "api_key",
  "value_env": "MY_API_KEY"
}
```

#### Basic Auth
```json
{
  "type": "basic",
  "username_env": "MY_USERNAME",
  "password_env": "MY_PASSWORD"
}
```

#### OAuth2 Client Credentials
```json
{
  "type": "oauth2_client_credentials",
  "token_url": "https://auth.example.com/oauth/token",
  "client_id_env": "MY_CLIENT_ID",
  "client_secret_env": "MY_CLIENT_SECRET",
  "scopes": ["read"]
}
```

### Pagination Strategies

#### Offset/Limit
Classic offset-based pagination. Increments offset by `page_size` each request.

```json
{
  "type": "offset_limit",
  "offset_param": "offset",
  "limit_param": "limit",
  "page_size": 100,
  "total_path": "$.meta.total"
}
```

Stop conditions: `total_path` exceeded, or page returns fewer items than `page_size`.

#### Page Number
Simple page number pagination. Increments page number each request.

```json
{
  "type": "page_number",
  "page_param": "page",
  "page_size_param": "per_page",
  "page_size": 100,
  "total_pages_path": "$.meta.total_pages"
}
```

Stop conditions: `total_pages_path` exceeded, or page returns fewer items than `page_size`.

#### Cursor (Stripe-style)
Extracts a cursor value from the response and passes it as a query parameter.

```json
{
  "type": "cursor",
  "cursor_param": "starting_after",
  "cursor_path": "$.data[-1].id",
  "has_more_path": "$.has_more",
  "page_size_param": "limit",
  "page_size": 100
}
```

Stop conditions: `has_more_path` is `false`, cursor is null/empty, or page is empty.

#### Link Header (GitHub/Shopify-style)
Follows the `rel="next"` URL in the RFC 5988 `Link` response header.

```json
{
  "type": "link_header",
  "page_size_param": "per_page",
  "page_size": 100
}
```

Stop conditions: no `Link` header with `rel="next"`, or page is empty.

#### Next URL (Salesforce/Twilio-style)
Extracts the next page URL from the response body.

```json
{
  "type": "next_url",
  "next_url_path": "$.nextRecordsUrl",
  "base_url": "https://instance.salesforce.com"
}
```

If the extracted URL is relative, `base_url` is prepended. Stop conditions: field is null/absent.

### Response Config

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `itemsPath` | string | `"$"` | JSONPath to the array of items in the response |
| `contentType` | string | auto-detect | Override content type (`json`, `xml`, `yaml`) |
| `mappings` | array | (required) | Element mapping configurations |

### Element Mapping

Each mapping produces a node or relation from each item:

```json
{
  "elementType": "node",
  "template": {
    "id": "{{item.id}}",
    "labels": ["User"],
    "properties": {
      "name": "{{item.name}}",
      "email": "{{item.email}}"
    }
  }
}
```

For relations, include `from` and `to`:

```json
{
  "elementType": "relation",
  "template": {
    "id": "{{item.id}}",
    "labels": ["FOLLOWS"],
    "from": "{{item.follower_id}}",
    "to": "{{item.following_id}}"
  }
}
```

Template values use [Handlebars](https://handlebarsjs.com/) syntax. The `item` variable contains the current response item.

---

## Real-World API Examples

### GitHub REST API

Fetch all repositories for an organization. Uses **Link header** pagination and **Bearer** token auth.

```json
{
  "endpoints": [
    {
      "url": "https://api.github.com/orgs/my-org/repos",
      "method": "GET",
      "headers": {
        "Accept": "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28"
      },
      "auth": {
        "type": "bearer",
        "token_env": "GITHUB_TOKEN"
      },
      "pagination": {
        "type": "link_header",
        "page_size_param": "per_page",
        "page_size": 100
      },
      "response": {
        "itemsPath": "$",
        "mappings": [
          {
            "elementType": "node",
            "template": {
              "id": "{{item.id}}",
              "labels": ["Repository"],
              "properties": {
                "name": "{{item.name}}",
                "full_name": "{{item.full_name}}",
                "description": "{{item.description}}",
                "language": "{{item.language}}",
                "stars": "{{item.stargazers_count}}",
                "forks": "{{item.forks_count}}",
                "private": "{{item.private}}"
              }
            }
          }
        ]
      }
    }
  ],
  "timeoutSeconds": 30,
  "maxRetries": 3,
  "retryDelayMs": 2000
}
```

### Shopify REST API

Fetch products from a Shopify store. Uses **Link header** pagination (with cursor in `page_info`) and **API key** header auth.

```json
{
  "endpoints": [
    {
      "url": "https://mystore.myshopify.com/admin/api/2024-01/products.json",
      "method": "GET",
      "auth": {
        "type": "api_key",
        "location": "header",
        "name": "X-Shopify-Access-Token",
        "value_env": "SHOPIFY_ACCESS_TOKEN"
      },
      "pagination": {
        "type": "link_header",
        "page_size_param": "limit",
        "page_size": 50
      },
      "response": {
        "itemsPath": "$.products",
        "mappings": [
          {
            "elementType": "node",
            "template": {
              "id": "{{item.id}}",
              "labels": ["Product"],
              "properties": {
                "title": "{{item.title}}",
                "vendor": "{{item.vendor}}",
                "product_type": "{{item.product_type}}",
                "status": "{{item.status}}",
                "created_at": "{{item.created_at}}"
              }
            }
          }
        ]
      }
    }
  ],
  "timeoutSeconds": 30,
  "maxRetries": 3,
  "retryDelayMs": 1000
}
```

### Stripe API

Fetch all customers. Uses **cursor-based** pagination (`starting_after` + `has_more`) and **Basic** auth (API key as username).

```json
{
  "endpoints": [
    {
      "url": "https://api.stripe.com/v1/customers",
      "method": "GET",
      "auth": {
        "type": "basic",
        "username_env": "STRIPE_SECRET_KEY",
        "password_env": null
      },
      "pagination": {
        "type": "cursor",
        "cursor_param": "starting_after",
        "cursor_path": "$.data[-1].id",
        "has_more_path": "$.has_more",
        "page_size_param": "limit",
        "page_size": 100
      },
      "response": {
        "itemsPath": "$.data",
        "mappings": [
          {
            "elementType": "node",
            "template": {
              "id": "{{item.id}}",
              "labels": ["Customer"],
              "properties": {
                "name": "{{item.name}}",
                "email": "{{item.email}}",
                "created": "{{item.created}}",
                "currency": "{{item.currency}}"
              }
            }
          }
        ]
      }
    }
  ],
  "timeoutSeconds": 60,
  "maxRetries": 5,
  "retryDelayMs": 2000
}
```

### Salesforce REST API

Fetch Accounts via SOQL query. Uses **next-URL** pagination (`nextRecordsUrl`) and **OAuth2 client credentials**.

```json
{
  "endpoints": [
    {
      "url": "https://myinstance.salesforce.com/services/data/v59.0/query?q=SELECT+Id,Name,Industry,AnnualRevenue+FROM+Account",
      "method": "GET",
      "auth": {
        "type": "oauth2_client_credentials",
        "token_url": "https://login.salesforce.com/services/oauth2/token",
        "client_id_env": "SF_CLIENT_ID",
        "client_secret_env": "SF_CLIENT_SECRET",
        "scopes": []
      },
      "pagination": {
        "type": "next_url",
        "next_url_path": "$.nextRecordsUrl",
        "base_url": "https://myinstance.salesforce.com"
      },
      "response": {
        "itemsPath": "$.records",
        "mappings": [
          {
            "elementType": "node",
            "template": {
              "id": "{{item.Id}}",
              "labels": ["Account"],
              "properties": {
                "name": "{{item.Name}}",
                "industry": "{{item.Industry}}",
                "annual_revenue": "{{item.AnnualRevenue}}"
              }
            }
          }
        ]
      }
    }
  ],
  "timeoutSeconds": 120,
  "maxRetries": 3,
  "retryDelayMs": 5000
}
```

### Twilio REST API

Fetch call records. Uses **next-URL** pagination (`next_page_uri`) and **Basic** auth (AccountSid:AuthToken).

```json
{
  "endpoints": [
    {
      "url": "https://api.twilio.com/2010-04-01/Accounts/ACXXXXXXXXX/Calls.json",
      "method": "GET",
      "auth": {
        "type": "basic",
        "username_env": "TWILIO_ACCOUNT_SID",
        "password_env": "TWILIO_AUTH_TOKEN"
      },
      "pagination": {
        "type": "next_url",
        "next_url_path": "$.next_page_uri",
        "base_url": "https://api.twilio.com"
      },
      "response": {
        "itemsPath": "$.calls",
        "mappings": [
          {
            "elementType": "node",
            "template": {
              "id": "{{item.sid}}",
              "labels": ["Call"],
              "properties": {
                "from": "{{item.from}}",
                "to": "{{item.to}}",
                "status": "{{item.status}}",
                "duration": "{{item.duration}}",
                "start_time": "{{item.start_time}}"
              }
            }
          }
        ]
      }
    }
  ],
  "timeoutSeconds": 60,
  "maxRetries": 3,
  "retryDelayMs": 1000
}
```

---

## Development

### Building

```bash
cargo build -p drasi-bootstrap-http
```

### Running Tests

```bash
# Unit tests
cargo test -p drasi-bootstrap-http --lib

# Integration tests (spins up real HTTP servers)
cargo test -p drasi-bootstrap-http --test integration_test
```

### Linting

```bash
cargo clippy -p drasi-bootstrap-http
```
