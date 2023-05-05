from influxdb_client import InfluxDBClient, Point

class Influx2(object):
    def __init__(self, config):
        self.influx_url = config['influx_url']
        self.influx_database = config['influx_database']
        self.influx_measurement = config['influx_measurement']
        self.influx_user = config['influx_user']
        self.influx_pass = config['influx_pass']
        self.influx_retention_policy = config['influx_retention_policy']

    def send(self, vals, batteryAPI):
        client = InfluxDBClient(url=self.influx_url, token=f'{self.influx_user}:{self.influx_pass}', org='-')
        bucket = f'{self.influx_database}/{self.influx_retention_policy}'
        write_api = client.write_api()

        inverterDetails = vals.copy()
        inverterDetails.pop('Serial', None)

        point = Point("solax").tag("inverter", vals['name'])
        for x, y in inverterDetails.items():
            point.field(x, y)
        #        print(point.to_line_protocol())
        #        print(inverterDetails)

        write_api.write(bucket=bucket, record=point)
        write_api.__del__()

        client.close()
