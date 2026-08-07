# Frontend (Next.js)

## Run Locally

```bash
npm install
npm run dev
```

Open http://localhost:3000

## Run With Docker (Frontend Only)

From the project root:

```bash
docker compose up --build frontend
```

Open http://localhost:3000

The frontend reads API base URL from `NEXT_PUBLIC_API_URL`.
In `docker-compose.yml`, it is set to `http://localhost:8000`.

## Notes

- API calls use `NEXT_PUBLIC_API_URL` (fallback: `http://localhost:8000`).
- To run full stack (db + backend + frontend), use:

```bash
docker compose up --build
```
