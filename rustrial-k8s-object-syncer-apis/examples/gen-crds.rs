use kube::CustomResourceExt;
use rustrial_k8s_object_syncer_apis::ObjectSync;
use serde_yaml;

pub fn main() {
    println!("{}", serde_yaml::to_string(&ObjectSync::crd()).unwrap());
}
