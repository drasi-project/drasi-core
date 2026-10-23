FROM python:3.12-slim-bookworm
WORKDIR /app
ARG PIP_INDEX_URL=https://pypi.org/simple
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt
COPY workflow.py demo.py ./
ENV PYTHONUNBUFFERED=1
CMD ["python", "workflow.py"]
