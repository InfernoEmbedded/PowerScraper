from paho.mqtt import client as mqtt

class Mqtt(object):
    def __init__(self, config):
        self.mqtt_host = config['mqtt_host']
        self.mqtt_port = config['mqtt_port']
        self.mqtt_keepalive = config['mqtt_keepalive']
        self.mqtt_user = config['mqtt_user']
        self.mqtt_pass = config['mqtt_pass']
        self.mqtt_topic = config['mqtt_topic']

    def on_connect(client, userdata, flags, rc):
        if rc != 0:
            print("Failed to connect, return code %d\n", rc)

    def send(self, vals):
        client = mqtt.Client()
        client.username_pw_set(username=self.mqtt_user, password=self.mqtt_pass)
        client.connect(self.mqtt_host, self.mqtt_port, self.mqtt_keepalive)

        inverterDetails = vals.copy()
        inverterDetails.pop('Serial', None)
        inverterDetails.pop('#SolaxClient', None)

        for x, y in inverterDetails.items():
            client.publish(f"{self.mqtt_topic}/{x}", y)
        #            print(f"Publish topic:{self.mqtt_topic}/{x} - Value:{y}")

        client.disconnect()

