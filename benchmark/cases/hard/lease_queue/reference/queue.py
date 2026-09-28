class Queue:
    def __init__(self):
        self.jobs = {}
        self.serial = 0

    def add(self, job_id):
        if job_id in self.jobs:
            raise ValueError("duplicate job")
        self.jobs[job_id] = {"deadline": None, "token": None, "attempts": 0}

    def claim(self, now, ttl):
        if ttl <= 0:
            raise ValueError("ttl must be positive")
        for job_id, job in self.jobs.items():
            if job["deadline"] is None or job["deadline"] <= now:
                self.serial += 1
                job["token"] = str(self.serial)
                job["deadline"] = now + ttl
                job["attempts"] += 1
                return job_id, job["token"]
        return None

    def ack(self, job_id, token):
        job = self.jobs.get(job_id)
        if not job or job["token"] != token:
            return False
        del self.jobs[job_id]
        return True
