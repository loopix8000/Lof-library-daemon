import uvicorn


if __name__ == "__main__":
    uvicorn.run("lof_daemon.app:create_app", factory=True, host="0.0.0.0", port=8787, workers=1)
