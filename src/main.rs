#![doc = include_str!("../Readme.md")]
#![warn(clippy::all, clippy::pedantic, clippy::cargo, clippy::nursery)]

use btleplug::{
    api::{Central, Manager as _, Peripheral, ScanFilter, WriteType},
    platform::{Adapter, Manager},
};
use bytes::{Buf, BufMut, Bytes};
use color_eyre::eyre::Result;
use futures::stream::StreamExt;
use std::time::Duration;
use tokio::time;

pub mod characteristics {
    use btleplug::api::{CharPropFlags, Characteristic};
    use uuid::{uuid, Uuid};

    // Aranet BLE uuids.
    // See <https://github.com/Anrijs/Aranet4-Python/blob/master/docs/UUIDs.md>
    // See <https://github.com/stijnstijn/pyaranet4/blob/f144d504434aa0d597c4694f659244561c225e3c/pyaranet4/pyaranet4.py#L32>
    const ARANET4_SERVICE: Uuid = uuid!("f0cd1400-95da-4f4b-9ac8-aa55d312af0c");
    const BLUETOOTH_SERVICE: Uuid = uuid!("0000180a-0000-1000-8000-00805f9b34fb");

    pub const SERIAL_NUMBER: Characteristic = Characteristic {
        service_uuid: BLUETOOTH_SERVICE,
        uuid:         uuid!("00002a25-0000-1000-8000-00805f9b34fb"),
        properties:   CharPropFlags::READ,
    };

    pub const CURRENT_READING_FULL: Characteristic = Characteristic {
        service_uuid: ARANET4_SERVICE,
        uuid:         uuid!("f0cd3001-95da-4f4b-9ac8-aa55d312af0c"),
        properties:   CharPropFlags::READ,
    };

    pub const STORED_READINGS: Characteristic = Characteristic {
        service_uuid: ARANET4_SERVICE,
        uuid:         uuid!("f0cd2001-95da-4f4b-9ac8-aa55d312af0c"),
        properties:   CharPropFlags::READ,
    };

    pub const HISTORY_RANGE: Characteristic = Characteristic {
        service_uuid: ARANET4_SERVICE,
        uuid:         uuid!("f0cd1402-95da-4f4b-9ac8-aa55d312af0c"),
        properties:   CharPropFlags::READ,
    };

    pub const HISTORY_NOTIFIER: Characteristic = Characteristic {
        service_uuid: ARANET4_SERVICE,
        uuid:         uuid!("f0cd2003-95da-4f4b-9ac8-aa55d312af0c"),
        properties:   CharPropFlags::READ.union(CharPropFlags::NOTIFY),
    };
}

#[allow(clippy::wildcard_imports)]
use characteristics::*;

#[derive(Clone, Copy, Debug)]
enum Sensor {
    Temperature,
    Humidity,
    Pressure,
    CO2,
}

impl Sensor {
    const fn id(self) -> u8 {
        match self {
            Self::Temperature => 1,
            Self::Humidity => 2,
            Self::Pressure => 3,
            Self::CO2 => 4,
        }
    }

    #[allow(clippy::cast_lossless)]
    fn read(self, reader: &mut impl Buf) -> f32 {
        match self {
            Self::Temperature => reader.get_u16_le() as f32 / 20.0,
            Self::Humidity => reader.get_u8() as f32,
            Self::Pressure => reader.get_u16_le() as f32 / 10.0,
            Self::CO2 => reader.get_u16_le() as f32,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let manager = Manager::new().await.unwrap();

    // get the first bluetooth adapter
    // TODO: support multiple adapters
    let adapters = manager.adapters().await?;
    let central = adapters.into_iter().next().unwrap();

    // start scanning for devices
    central.start_scan(ScanFilter::default()).await?;

    // instead of waiting, you can use central.events() to get a stream which will
    // notify you of new devices, for an example of that see
    // examples/event_driven_discovery.rs
    time::sleep(Duration::from_secs(2)).await;

    // find the device we're interested in
    find_aranets(&central).await?;

    Ok(())
}

async fn find_aranets(central: &Adapter) -> Result<()> {
    for p in central.peripherals().await? {
        if let Some(props) = p.properties().await? &&
         let Some(name) = &props.local_name &&
         name.starts_with("Aranet4") {
            dbg!(&props);
            p.connect().await?;
            p.discover_services().await?;
            dbg!(&p.characteristics());

            read_aranet(p).await?;
         }
    }
    Ok(())
}

async fn read_aranet(p: impl Peripheral) -> Result<()> {
    let serial = p.read(&SERIAL_NUMBER).await?;
    dbg!(&serial);

    let result = p.read(&CURRENT_READING_FULL).await?;
    let mut reader = &result[..];
    println!("CO2 = {}", Sensor::CO2.read(&mut reader));
    println!("Temperature = {}", Sensor::Temperature.read(&mut reader));
    println!("Pressure = {}", Sensor::Pressure.read(&mut reader));
    println!("Humidity = {}", Sensor::Humidity.read(&mut reader));
    println!("Battery = {}", reader.get_u8());
    println!("Status = {}", reader.get_u8());
    println!("Interval = {}", reader.get_u16_le());
    println!("Passed = {}", reader.get_u16_le());

    println!(
        "Temperature = {:?}",
        read_history(&p, Sensor::Temperature).await?
    );
    println!("Pressure = {:?}", read_history(&p, Sensor::Pressure).await?);
    println!("Humidity = {:?}", read_history(&p, Sensor::Humidity).await?);
    println!("CO2 = {:?}", read_history(&p, Sensor::CO2).await?);

    Ok(())
}

async fn read_history(p: &impl Peripheral, sensor: Sensor) -> Result<Vec<f32>> {
    // This will trigger a pairing request.
    let data = p.read(&STORED_READINGS).await?;
    let mut reader = &data[..];
    let num_samples = reader.get_u16_le();
    dbg!(num_samples);

    // Fetch history range.
    // 8200 0000 0100 ffff
    let mut data = [0_u8; 8];
    let mut writer = &mut data[..];
    writer.put_u8(0x82); // ?
    writer.put_u8(sensor.id());
    writer.put_u16_le(0); // ?
    writer.put_u16_le(1); // start
    writer.put_u16_le(0xffff); // end
    p.write(&HISTORY_RANGE, &data, WriteType::WithoutResponse)
        .await?;
    dbg!(Bytes::from(data.to_vec()));

    let mut samples = vec![f32::NAN; num_samples as usize];
    let mut samples_read = 0;

    // Derive sample timestamps from the interval and time passed values.
    p.subscribe(&HISTORY_NOTIFIER).await?;
    let mut notifications = p.notifications().await?;
    while let Some(notification) = notifications.next().await {
        if notification.uuid != HISTORY_NOTIFIER.uuid {
            continue;
        }
        let mut reader = &notification.value[..];
        let sensor_id = reader.get_u8();
        let index = reader.get_u16_le();
        let length = reader.get_u8();
        dbg!((sensor, index, length));
        assert_eq!(sensor_id, sensor.id());
        for i in index as usize..index as usize + length as usize {
            samples[i - 1] = sensor.read(&mut reader);
            samples_read += 1;
        }
        if samples_read == num_samples as usize {
            break;
        }
    }
    Ok(samples)
}
